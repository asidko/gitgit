//! The real git backend, reading an on-disk repository via libgit2 (`git2`).
//!
//! This is the ONLY module that imports `git2`; its types never escape this file.
//! Because a `git2::Repository` (and the `Commit`/`Tree`/`Diff` borrowed from it)
//! is `!Send` and not `'static`, the struct stores only a `PathBuf` - every trait
//! method opens its OWN short-lived `Repository`, extracts fully-owned `model`/
//! `diff` types, and drops the libgit2 handles before returning. That keeps
//! `RealBackend: Send + Sync` so a later off-thread loader can hold it.
//!
//! It returns RAW (un-highlighted) `FileView`s: syntax highlighting is the
//! loader's single `crate::tokenize::highlight_view` pass, NOT done here.

use std::path::PathBuf;

use git2::{
    ApplyLocation, ApplyOptions, Commit, Delta, DiffOptions, Oid, Patch, Repository, Sort, Tree,
};

use crate::config::{Config, DateFormat};
use crate::diff::{DiffLine, FileDiff, FileView, LineKind, SourceFile, Token, TokenKind};
use crate::model::{
    Commit as CommitRow, CommitDetail, FileStatus, Ref, RefKind, Signature, SubjectSpan,
    SubjectTone, TreeNode,
    WORKING_REV,
};

use super::build::{format_date, format_when, format_when_relative, subject_spans, tree_from_paths};
use super::{BackendError, GitBackend, RepoSnapshot, RevertOutcome};

/// Length of the abbreviated commit hash shown in the log (matches the fixtures).
const SHORT_HASH_LEN: usize = 8;

/// Cap on a file's revision-history list (the Compare-with-Revision picker): a deep history
/// stays bounded so the picker walk + render never blow up on a long-lived file.
const FILE_REVISIONS_CAP: usize = 200;

/// Cap on COMMITS VISITED while building that list. `FILE_REVISIONS_CAP` bounds the matching
/// rows, but a RARELY-changed file in a deep-history repo would otherwise walk the entire
/// history (two tree lookups per commit) before collecting 200 hits. This bounds the scan
/// itself so the picker appears promptly regardless of how seldom the file changed.
const FILE_REVISIONS_SCAN_CAP: usize = 5000;

/// Reads an on-disk repository via libgit2. Holds only the path + the boot config's
/// behavior knobs (commit/preview caps + date format): each method opens its own
/// `Repository` (see module docs), so the struct stays `Send + Sync`.
///
/// The caps and date format come from `[behavior]`; their DEFAULTS (the prior
/// `COMMIT_CAP=300`, `PREVIEW_LINE_CAP=5000`, `"DD.MM.YYYY, HH:MM"`) live once in
/// `config::Config::default`, the single source - this struct just carries the
/// resolved values.
pub(crate) struct RealBackend {
    path: PathBuf,
    /// Cap on commits walked in one `load_repo`. Interior-mutable so "Load more history"
    /// (`load_more`) can grow it through the shared `&self` trait object; a later `Reload`
    /// keeps the grown cap.
    commit_cap: std::sync::atomic::AtomicUsize,
    /// Cap on lines emitted for one diff/source preview (guards huge files).
    preview_line_cap: usize,
    /// Commit-date rendering format (default `DD.MM.YYYY, HH:MM`).
    date_fmt: DateFormat,
}

impl RealBackend {
    /// Open `path` as a git repository with the boot `config`'s behavior knobs,
    /// validating the path up front so a bad path fails here rather than on first
    /// use. The handle is dropped immediately; methods re-open as needed.
    pub(crate) fn open(path: impl Into<PathBuf>, config: &Config) -> Result<Self, BackendError> {
        let path = path.into();
        Repository::open(&path).map_err(to_err)?;
        Ok(RealBackend {
            path,
            // Clamp the caps to a sane minimum: a 0 (typo'd) commit_cap would walk no
            // commits and silently render a blank log over a non-empty repo; a 0
            // preview_line_cap would show an empty preview for every file. Both bypass
            // the layout/column clamps, so they are clamped here at the consumer.
            commit_cap: std::sync::atomic::AtomicUsize::new(config.behavior.commit_cap.max(1)),
            preview_line_cap: config.behavior.preview_line_cap.max(1),
            date_fmt: config.behavior.date_format,
        })
    }

    /// Open this backend's repository for one method call.
    fn repo(&self) -> Result<Repository, BackendError> {
        Repository::open(&self.path).map_err(to_err)
    }

    /// The repository's working-directory path, where the system `git` runs for the
    /// repo-level writes. Errors on a bare repo (no working tree).
    fn workdir_path(&self) -> Result<PathBuf, BackendError> {
        let repo = self.repo()?;
        repo.workdir()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| BackendError("repository has no working tree (bare)".to_string()))
    }

    /// The local-changes diff under `file` (a FILE target or a DIRECTORY prefix) as RAW bytes
    /// (byte-exact: trailing newline kept so `git apply` accepts it). Tracked changes come from
    /// `git diff HEAD`; every brand-new UNTRACKED file under the path (which `git diff HEAD`
    /// omits) is diffed against /dev/null and appended, so a folder patch captures both its
    /// modified and new files. A tracked-but-unchanged file stays empty (NOT a spurious
    /// full-add). The String-returning `working_patch` lossy-wraps this for the clipboard.
    fn working_patch_bytes(&self, file: &str) -> Result<Vec<u8>, BackendError> {
        let dir = self.workdir_path()?;
        // `:(literal)` forces a LITERAL pathspec so glob magic (`*?[`, a leading `:`) in a
        // name like `[id].tsx` matches the path, not a pattern; `--` fences it off. The pathspec
        // works for a FILE target AND a DIRECTORY prefix (a folder Copy/Create Patch).
        // An EMPTY `file` means the WHOLE working tree (the global Git menu's "Create Patch from
        // local changes"): omit the `-- <spec>` fence so the diff + untracked sweep cover every
        // path. A non-empty `file` keeps the literal pathspec (a single file or a folder prefix).
        let whole_tree = file.is_empty();
        let spec = literal_pathspec(file);
        let pathspec: &[&str] = if whole_tree { &[] } else { &["--", &spec] };
        // Tracked + staged changes under the spec. `-c core.quotePath=false` keeps a non-ASCII
        // path readable in the patch header (else git C-quotes it to octal; it applies either
        // way). `git diff HEAD` errors on an UNBORN HEAD (fresh zero-commit repo), so there use
        // `git diff --cached` (index vs the empty tree) - a STAGED-but-uncommitted file still
        // yields an add patch. The untracked sweep below covers the new (unstaged) files.
        let has_head = run_git(&dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok();
        let base = if has_head { "HEAD" } else { "--cached" };
        let diff_args = [&["-c", "core.quotePath=false", "diff", base][..], pathspec].concat();
        let mut out = run_git_bytes(&dir, &diff_args)?;
        // Untracked NEW files under the spec (a single untracked file, OR new files in a folder
        // that the tracked diff above left out): diff each against /dev/null and append, so the
        // patch is COMPLETE - a folder patch captures both its modified and its brand-new files.
        // `-z` emits NUL-delimited UNQUOTED paths: without it `core.quotePath` (default on)
        // C-quotes any non-ASCII / control / quote / backslash name (`"na\303\257ve.js"`), and
        // that quoted literal would not resolve for `git diff --no-index`, aborting the patch.
        let others_args = [&["ls-files", "--others", "--exclude-standard", "-z"][..], pathspec].concat();
        let others = run_git_bytes(&dir, &others_args)?;
        for seg in others.split(|&b| b == 0).filter(|s| !s.is_empty()) {
            // Skip a non-UTF-8 name (legal on Linux) rather than aborting the WHOLE patch on one
            // odd sibling - the rest of the folder's files still produce a usable patch.
            if let Ok(path) = std::str::from_utf8(seg) {
                out.extend(run_git_diff_no_index(&dir, path)?);
            }
        }
        Ok(out)
    }
}

impl GitBackend for RealBackend {
    fn load_repo(&self) -> Result<RepoSnapshot, BackendError> {
        let repo = self.repo()?;
        let current_user = config_user_name(&repo);
        let refs = ref_map(&repo)?;
        let head_oid = head_oid(&repo);
        // Load-time wall clock for relative ("Today"/"Yesterday") log dates. 0 if the
        // clock is unavailable, which the formatter renders as plain absolute dates.
        let now_epoch = now_unix();

        // Topological + time order so the log reads newest-first along the trunk,
        // matching `git log --topo-order`.
        let mut walk = repo.revwalk().map_err(to_err)?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)
            .map_err(to_err)?;
        // Walk EVERY branch tip (local + remote-tracking) plus HEAD so the log shows
        // all branches' commits like `git log --all`, not just the current branch's
        // history - divergent commits must be visible for cherry-pick and other
        // cross-branch ops to reach them. push_glob is a no-op when nothing matches and
        // push_head errs on an unborn repo, so emptiness is decided below by whether the
        // walk yielded any commit (not by these calls). push_head also covers a detached
        // HEAD (on no branch).
        let _ = walk.push_glob("refs/heads/*");
        let _ = walk.push_glob("refs/remotes/*");
        let _ = walk.push_head();

        let cap = self.commit_cap.load(std::sync::atomic::Ordering::Relaxed);
        let mut commits: Vec<CommitRow> = Vec::new();
        // `truncated` = the walk reached a commit PAST the cap (more history exists), which
        // drives the log's "Load more history" row. Breaking ON that commit means we saw it
        // but did not keep it, so there is at least one more to load.
        let mut truncated = false;
        for oid in walk {
            if commits.len() >= cap {
                truncated = true;
                break;
            }
            let oid = oid.map_err(to_err)?;
            let commit = repo.find_commit(oid).map_err(to_err)?;
            commits.push(commit_row(&commit, &refs, head_oid, self.date_fmt, now_epoch));
        }
        // Working-tree status, computed up front: it feeds the synthetic `<current>` row
        // below AND the change-detection signature (which the empty-repo path needs too -
        // an unborn repo can still gain untracked files the poll must notice).
        let working = working_status_list(&repo).unwrap_or_default();
        let status_sig = working_sig(head_oid, &working);

        // No commit reachable from any ref -> an unborn/empty repo, not an error.
        if commits.is_empty() {
            return Ok(empty_snapshot(current_user, status_sig));
        }

        // Stamp the FULL branch membership on every loaded commit so the Branch
        // filter selects a branch's whole reachable history (not just its tip) and
        // its dropdown lists every branch overlapping the window. One revwalk per
        // branch over the loaded set, NOT a descendant check per (commit, branch).
        stamp_containing_branches(&repo, &mut commits)?;

        // A synthetic "<current>" row is ALWAYS pinned at the top (so working state is
        // surfaced first and is the live-edit entry point). Dirty tree -> its subject is
        // the "+N ~N -N" badge and its tree IS the working status (each file a live diff
        // vs HEAD); clean tree -> "<current>  no changes" with an empty tree. Startup
        // opens ON it either way.
        // The <current> row's diff base + graph parent is the ACTUAL HEAD - derived from
        // `head_oid`, NOT `commits.first()`: since the revwalk walks all branches in
        // TOPOLOGICAL|TIME order, the first yielded commit is the globally newest tip,
        // which need not be HEAD (a sibling branch can have a newer-timestamped commit).
        let head_short = head_oid.map(short_hash);
        // The branch HEAD is on (None when detached), so the <current> row shows its
        // "<diamond> branch" chip matching the log's local-branch decorations.
        let current_branch = head_branch(&repo);
        let row = working_row(&working, head_short, &current_user, current_branch.as_deref());
        let tree = tree_from_paths(&working);
        let mut all = Vec::with_capacity(commits.len() + 1);
        all.push(row);
        all.extend(commits);
        let (commits, tree) = (all, tree);

        // Preselect the first FILE row (not a directory) so the startup preview
        // shows a real diff rather than a folder. Falls back to row 0 when the
        // tree is all directories / empty.
        let default_selection = first_file_row(&tree);

        let unpushed = unpushed_hashes(&repo, &commits);
        let has_remotes = has_remote_refs(&repo);
        Ok(RepoSnapshot {
            commits,
            tree,
            default_selection,
            current_user,
            unpushed,
            has_remotes,
            truncated,
            status_sig,
        })
    }

    fn load_more(&self) -> Result<RepoSnapshot, BackendError> {
        // Double the commit cap, then re-walk: each "Load more" loads twice as deep. A later
        // `Reload` keeps the grown cap (the next watch tick will not snap back to the old slice).
        // One atomic RMW (not load-then-store): the loader is single-threaded today, but a
        // second worker must not be able to lose a doubling.
        use std::sync::atomic::Ordering::Relaxed;
        self.commit_cap
            .fetch_update(Relaxed, Relaxed, |cap| Some(cap.saturating_mul(2)))
            .ok();
        self.load_repo()
    }

    fn status_sig(&self) -> Result<u64, BackendError> {
        let repo = self.repo()?;
        let working = working_status_list(&repo)?;
        Ok(working_sig(head_oid(&repo), &working))
    }

    fn commit_detail(&self, hash: &str) -> Result<CommitDetail, BackendError> {
        let repo = self.repo()?;
        // The synthetic working row has no real commit: its detail is the uncommitted
        // summary (no hash chip, no "In N branches"), built from the same counts.
        if hash == WORKING_REV {
            let working = working_status_list(&repo).unwrap_or_default();
            let row = working_row(
                &working,
                None,
                &config_user_name(&repo),
                head_branch(&repo).as_deref(),
            );
            // `working_row` already stamped the summary onto the row, so the cheap
            // detail carries it (the store's rebuild path agrees with this).
            return Ok(crate::model::detail_from(&row));
        }
        let commit = find_commit(&repo, hash)?;
        let author = signature(&commit.author(), self.date_fmt);
        let committer = signature(&commit.committer(), self.date_fmt);
        Ok(CommitDetail {
            subject: summary(&commit).to_string(),
            short_hash: short_hash(commit.id()),
            author,
            committer,
            // EXPENSIVE, hence per-commit and lazy: never run in load_repo.
            containing_branches: containing_branches(&repo, commit.id())?,
            working: None,
        })
    }

    fn file_view(&self, commit: &str, path: &str) -> Result<Option<FileView>, BackendError> {
        let repo = self.repo()?;
        let commit = find_commit(&repo, resolve_rev(commit))?;
        let new_tree = commit.tree().map_err(to_err)?;
        let parent = mainline_parent(&commit);
        let old_tree = parent_tree(&parent)?;

        // Changed in this commit -> a per-file Patch diff; otherwise the unchanged
        // file's source (if it exists at this commit), else no preview.
        if delta_status(&repo, old_tree.as_ref(), &new_tree, path)?.is_some() {
            return Ok(Some(diff_view(
                &repo,
                old_tree.as_ref(),
                &new_tree,
                path,
                &commit,
                &parent,
                self.preview_line_cap,
            )?));
        }
        source_view(&repo, &new_tree, path, self.preview_line_cap)
    }

    fn revision_source(&self, rev: &str, path: &str) -> Result<Option<FileView>, BackendError> {
        // The file's CONTENT at `rev` (not a diff): resolve the rev, then read its blob as a
        // read-only source. `source_view` returns Ok(None) when the path is absent/binary at
        // that revision, which the overlay surfaces as a "no revision" Notice.
        let repo = self.repo()?;
        let commit = find_commit(&repo, resolve_rev(rev))?;
        let tree = commit.tree().map_err(to_err)?;
        source_view(&repo, &tree, path, self.preview_line_cap)
    }

    fn compare_view(&self, base: &str, rev: &str, path: &str) -> Result<Option<FileView>, BackendError> {
        // Diff the picked rev's blob (old/left side) against `base` (new/right side), pathspec'd
        // to `path`. `base` == WORKING_REV diffs the live working tree (the `<current>` row);
        // any other base diffs that commit's blob (a historical row). Reuses the read-only diff
        // processing (fold markers + inline word highlight) so a compare reads like a real diff.
        let repo = self.repo()?;
        let commit = find_commit(&repo, resolve_rev(rev))?;
        let old_tree = commit.tree().map_err(to_err)?;
        // The new side: a commit's tree (historical base) or the working tree (WORKING_REV).
        let base_commit = if base == WORKING_REV {
            None
        } else {
            Some(find_commit(&repo, resolve_rev(base))?)
        };
        let new_tree = base_commit.as_ref().map(Commit::tree).transpose().map_err(to_err)?;
        let mut opts = DiffOptions::new();
        opts.pathspec(path);
        // FULL context (see `diff_view`): the whole file, no gap markers - "Hide unchanged"
        // folds it in the View layer so a compare/inspect diff folds like every other diff.
        opts.context_lines(u32::MAX);
        let diff = match &new_tree {
            Some(nt) => repo.diff_tree_to_tree(Some(&old_tree), Some(nt), Some(&mut opts)),
            None => repo.diff_tree_to_workdir(Some(&old_tree), Some(&mut opts)),
        }
        .map_err(to_err)?;
        let new_rev = base_commit.as_ref().map(|c| short_hash(c.id())).unwrap_or_else(|| "working".to_string());
        match patch_lines_or_binary(&diff, self.preview_line_cap)? {
            PatchBody::Binary => Ok(Some(binary_view(path))),
            // Identical sides: show the file's content read-only (a full-width Source) instead
            // of an empty two-pane diff.
            PatchBody::Lines(lines) if lines.is_empty() => {
                source_view(&repo, &old_tree, path, self.preview_line_cap)
            }
            PatchBody::Lines(lines) => Ok(Some(FileView::Diff(FileDiff {
                path: path.to_string(),
                old_rev: short_hash(commit.id()),
                new_rev,
                lines,
            }))),
        }
    }

    fn blame(&self, rev: &str, path: &str) -> Result<Option<FileView>, BackendError> {
        // Shell out to `git blame --porcelain`: git2's blame_file cannot annotate the dirty
        // working tree (uncommitted lines), and the porcelain stream gives author + time per
        // line directly. WORKING_REV omits the rev so git blames the live file; any other rev
        // blames that commit. An untracked / absent path exits nonzero -> no overlay (Ok(None)).
        // `git blame` does NOT honor `:(literal)` pathspec magic (it reads a single pathname), so
        // pass the bare path under the top-level `--literal-pathspecs` flag - that matches it
        // literally (no glob), the protection the magic would have given a glob-char filename.
        let dir = self.workdir_path()?;
        let mut args: Vec<&str> =
            vec!["--literal-pathspecs", "-c", "core.quotePath=false", "blame", "--porcelain"];
        if rev != WORKING_REV {
            args.push(rev);
        }
        args.push("--");
        args.push(path);
        let Ok(out) = run_git_bytes(&dir, &args) else {
            return Ok(None);
        };
        let lines = parse_blame_porcelain(&out, self.date_fmt);
        if lines.is_empty() {
            return Ok(None);
        }
        Ok(Some(FileView::Blame(crate::diff::BlameFile { path: path.to_string(), lines })))
    }

    fn file_revisions(&self, path: &str) -> Result<Vec<(String, String)>, BackendError> {
        // Walk history newest-first; a commit is in `path`'s history when the path's blob differs
        // from the FIRST parent's (added/modified/deleted) - the simplified `git log -- <path>`.
        let repo = self.repo()?;
        let mut walk = repo.revwalk().map_err(to_err)?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME).map_err(to_err)?;
        walk.push_head().map_err(to_err)?;
        let spec = std::path::Path::new(path);
        let blob_at = |c: &Commit| c.tree().ok().and_then(|t| t.get_path(spec).ok().map(|e| e.id()));
        let mut out = Vec::new();
        for (visited, oid) in walk.enumerate() {
            if out.len() >= FILE_REVISIONS_CAP || visited >= FILE_REVISIONS_SCAN_CAP {
                break;
            }
            let commit = repo.find_commit(oid.map_err(to_err)?).map_err(to_err)?;
            let here = blob_at(&commit);
            let parent = commit.parent(0).ok().and_then(|p| blob_at(&p));
            if here != parent {
                let short = short_hash(commit.id());
                let subject = summary(&commit);
                // `hash  date  subject`: hash + date form a stable prefix so two commits with
                // the SAME subject (repeated `wip` / `fix typo`) are still distinguishable.
                let when = commit.author().when();
                let date = format_date(when.seconds(), when.offset_minutes(), self.date_fmt);
                out.push((short.clone(), format!("{short}  {date}  {subject}")));
            }
        }
        Ok(out)
    }

    fn list_refs(&self) -> Result<Vec<(String, String)>, BackendError> {
        // Branches (local + remote) and tags, sorted; HEAD itself is skipped (it is not a target
        // you compare against). Each row resolves by name through `find_commit`'s revparse.
        let repo = self.repo()?;
        let mut out = Vec::new();
        for r in repo.references().map_err(to_err)? {
            let r = r.map_err(to_err)?;
            let Some((kind, name)) = classify_ref(&r) else { continue };
            let tag = match kind {
                RefKind::LocalBranch => "branch",
                RefKind::RemoteBranch => "remote",
                RefKind::Tag => "tag",
                RefKind::Head => continue,
            };
            out.push((name.clone(), format!("{name}  ({tag})")));
        }
        out.sort();
        Ok(out)
    }

    fn changed_files(&self, hash: &str) -> Result<Vec<TreeNode>, BackendError> {
        let repo = self.repo()?;
        // The "<current>" row lists the working-tree status, not a commit's changeset.
        if hash == WORKING_REV {
            return Ok(tree_from_paths(&working_status_list(&repo)?));
        }
        changed_files_in(&repo, hash)
    }

    fn full_tree(&self, hash: &str) -> Result<Vec<TreeNode>, BackendError> {
        let repo = self.repo()?;
        // The "<current>" row's full tree overlays the WORKING status (vs HEAD), NOT a
        // commit's own diff-vs-parent: resolving it to HEAD would highlight the last
        // commit's files as changed (and leave them lit after a commit lands).
        if hash == WORKING_REV {
            return working_full_tree_in(&repo);
        }
        full_tree_in(&repo, resolve_rev(hash))
    }

    fn ignored_paths(&self, hash: &str) -> Result<std::collections::HashSet<String>, BackendError> {
        let repo = self.repo()?;
        let tree = full_tree_in(&repo, resolve_rev(hash))?;
        // Walk BOTH files and (collapsed) directories: is_path_ignored evaluates the
        // .gitignore rules regardless of tracking, so a committed/force-added file - or
        // a whole ignored directory - that matches an ignore pattern is reported. Dir
        // paths use the SAME collapsed strings the files panel reconstructs, so the
        // panel's per-row lookup matches.
        let mut out = std::collections::HashSet::new();
        collect_ignored(&repo, &tree, "", &mut out);
        Ok(out)
    }

    fn revert_file(&self, commit: &str, path: &str) -> Result<RevertOutcome, BackendError> {
        let repo = self.repo()?;
        revert_file_in(&repo, commit, path)
    }

    fn revert_hunk(&self, commit: &str, path: &str, hunk: usize) -> Result<RevertOutcome, BackendError> {
        let repo = self.repo()?;
        revert_hunk_in(&repo, commit, path, hunk)
    }

    fn read_worktree(&self, path: &str) -> Result<String, BackendError> {
        let repo = self.repo()?;
        let workdir = repo
            .workdir()
            .ok_or_else(|| BackendError("repository has no working tree (bare)".to_string()))?;
        let src = safe_join(workdir, path)?;
        let bytes = std::fs::read(&src).map_err(io_err)?;
        String::from_utf8(bytes).map_err(|_| BackendError(format!("not a UTF-8 text file: {path}")))
    }

    fn read_commit_file(&self, commit: &str, path: &str) -> Result<Option<String>, BackendError> {
        let repo = self.repo()?;
        let tree = find_commit(&repo, resolve_rev(commit))?.tree().map_err(to_err)?;
        let spec = std::path::Path::new(path);
        match tree_blob_bytes(&repo, Some(&tree), spec)? {
            Some(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| BackendError(format!("not a UTF-8 text file: {path}"))),
            None => Ok(None), // path not present at that commit
        }
    }

    fn write_worktree(&self, path: &str, content: &str) -> Result<(), BackendError> {
        let repo = self.repo()?;
        let workdir = repo
            .workdir()
            .ok_or_else(|| BackendError("repository has no working tree (bare)".to_string()))?;
        let dest = safe_join(workdir, path)?;
        write_workfile(&dest, content.as_bytes())
    }

    // Repo-level writes shell out to the system `git` so they honor the user's hooks,
    // config, and (for push/pull) their existing remote credentials / ssh-agent - the
    // git2 build here has no network transport. All run on the loader thread.

    fn commit(&self, message: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        run_git(&dir, &["add", "-A"])?;
        run_git(&dir, &["commit", "-m", message])?;
        Ok("Committed".to_string())
    }

    fn amend(&self, message: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        run_git(&dir, &["add", "-A"])?;
        // The dialog edits only the SUBJECT line; re-attach HEAD's existing body so an amend
        // never silently drops the commit's message paragraphs (mirrors reword_at). `-F` reads
        // the full multi-line message from a temp file; no `--only`, so the staged working
        // changes ARE folded into HEAD (that is what amend is for).
        let full = compose_message(&dir, "HEAD", message)?;
        let msg_path = write_temp_message("HEAD", &full)?;
        let result = run_git(&dir, &["commit", "--amend", "-F", &path_arg(&msg_path)])
            .map(|_| "Amended HEAD".to_string());
        let _ = std::fs::remove_file(&msg_path);
        result
    }

    fn tag(&self, name: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        run_git(&dir, &["tag", name])?;
        Ok(format!("Tagged {name}"))
    }

    fn branch_create(&self, name: &str, commit: &str, checkout: bool) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(name)?;
        if checkout {
            // `checkout -b` moves HEAD + the working tree to `commit`; with a dirty tree
            // and a different target git refuses (error surfaces as a Notice).
            run_git(&dir, &["checkout", "-b", name, commit])?;
            Ok(format!("Created and checked out {name}"))
        } else {
            run_git(&dir, &["branch", name, commit])?;
            Ok(format!("Created branch {name}"))
        }
    }

    fn tag_at(&self, name: &str, commit: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(name)?;
        run_git(&dir, &["tag", name, commit])?;
        Ok(format!("Tagged {name}"))
    }

    fn reword_at(&self, commit: &str, subject: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // The dialog edits only the SUBJECT line; re-attach the commit's existing body
        // so a reword never drops message paragraphs.
        let message = compose_message(&dir, commit, subject)?;
        let msg_path = write_temp_message(commit, &message)?;
        let result = if is_head_commit(&dir, commit)? {
            // HEAD -> message-only amend. `--only` with no pathspec amends using HEAD's
            // tree, IGNORING the index, so a reword stays message-only even if changes
            // are staged. `-F` reads the full (multi-line) message from the file.
            run_git(&dir, &["commit", "--amend", "--only", "-F", &path_arg(&msg_path)])
                .map(|_| "Reworded HEAD".to_string())
        } else {
            reword_via_rebase(&dir, commit, &msg_path)
        };
        let _ = std::fs::remove_file(&msg_path);
        result
    }

    fn checkout(&self, commit: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // A raw hash detaches HEAD; git refuses when the switch would overwrite
        // uncommitted changes (that error surfaces as a Notice). No `add -A` - checkout
        // must not touch the index/working tree beyond the switch itself.
        run_git(&dir, &["checkout", commit])?;
        Ok(format!("Checked out {} (detached HEAD)", short_str(commit)))
    }

    fn cherry_pick(&self, commit: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        apply_or_abort(
            &dir,
            &["cherry-pick", commit],
            &["cherry-pick", "--abort"],
            format!("Cherry-picked {}", short_str(commit)),
        )
    }

    fn cherry_pick_multi(&self, commits: &[String]) -> Result<String, BackendError> {
        if commits.is_empty() {
            return Err(BackendError("no commits selected to cherry-pick".to_string()));
        }
        let dir = self.workdir_path()?;
        // `git cherry-pick <h1> <h2> ...` applies them in the GIVEN order (the store passes them
        // oldest-first); a conflict mid-series aborts the WHOLE pick so the branch never sticks.
        let mut args = vec!["cherry-pick"];
        args.extend(commits.iter().map(String::as_str));
        apply_or_abort(&dir, &args, &["cherry-pick", "--abort"], format!("Cherry-picked {} commits", commits.len()))
    }

    fn create_patch_series(&self, commits: &[String], dir_path: &str) -> Result<String, BackendError> {
        if commits.is_empty() {
            return Err(BackendError("no commits selected for the patch series".to_string()));
        }
        let dir = self.workdir_path()?;
        std::fs::create_dir_all(dir_path).map_err(io_err)?;
        // Format EACH commit on its own (`-1 <commit>`) with an incrementing `--start-number`,
        // so the result is an EXACT, numbered series. A single multi-rev `format-patch` call
        // would read the SHAs as a revision range (or, with `^!`, let one commit's parent
        // exclusion drop an ancestor sibling), so per-commit emission is the only exact way.
        for (i, c) in commits.iter().enumerate() {
            let n = (i + 1).to_string();
            run_git(&dir, &["format-patch", "-o", dir_path, "--start-number", &n, "-1", c])?;
        }
        Ok(format!("Wrote {} patch(es) to {dir_path}", commits.len()))
    }

    fn revert_commit(&self, commit: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // `--no-edit` keeps the default "Revert <subject>" message (no editor); a
        // conflict aborts so no half-applied revert is left in the working tree.
        apply_or_abort(
            &dir,
            &["revert", "--no-edit", commit],
            &["revert", "--abort"],
            format!("Reverted {}", short_str(commit)),
        )
    }

    fn reset(&self, commit: &str, mode_flag: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        run_git(&dir, &["reset", mode_flag, commit])?;
        // Name the mode without the leading dashes for the status notice.
        Ok(format!("Reset to {} ({})", short_str(commit), mode_flag.trim_start_matches('-')))
    }

    fn undo_commit(&self) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // Soft reset the tip: move HEAD back one, keeping the commit's changes staged.
        run_git(&dir, &["reset", "--soft", "HEAD~1"])?;
        Ok("Undid the last commit (changes kept staged)".to_string())
    }

    fn stash(&self) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // `--include-untracked` also stashes new files; git errors with "No local changes
        // to save" on a clean tree (surfaced as a Notice).
        run_git(&dir, &["stash", "push", "--include-untracked"])?;
        Ok("Stashed working changes (restore with git stash pop)".to_string())
    }

    fn discard_all(&self) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // Hard-reset the working tree + index to HEAD (discards tracked modifications and
        // staged changes), THEN `git clean -fd` to remove the new/untracked files + dirs the
        // `<current>` row counts as added - so discard clears EVERY uncommitted change, not just
        // tracked edits. `-d` includes untracked directories; no `-x`, so `.gitignore`d build
        // artifacts are deliberately preserved (only the user's added files are dropped).
        run_git(&dir, &["reset", "--hard", "HEAD"])?;
        run_git(&dir, &["clean", "-fd"])?;
        Ok("Discarded all uncommitted changes".to_string())
    }

    fn fetch(&self) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // Download remote commits/refs without integrating. No remote -> git errors
        // (surfaced as a Notice).
        run_git(&dir, &["fetch", "--all", "--prune"])?;
        Ok("Fetched from remote".to_string())
    }

    fn update_project(&self) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // WebStorm's "Update Project": fetch THEN integrate, but never hard-fail on a
        // local-only repo or an upstream-less branch. Fetch first (prune stale remotes);
        // no remote configured -> nothing to update, report and stop.
        if run_git(&dir, &["fetch", "--all", "--prune"]).is_err() {
            return Ok("No remote to update from".to_string());
        }
        // Pull only when the current branch tracks an upstream. A branch never pushed (or a
        // remote without this branch) has no `@{u}` - the fetch already refreshed remote refs,
        // so skip the pull with a notice instead of erroring.
        if run_git(&dir, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).is_err() {
            return Ok("Fetched; current branch has no upstream to pull".to_string());
        }
        // Fast-forward when possible (no merge commit); on a diverged branch ff is refused, so
        // fall back to a rebase that replays local commits on top. A conflicting rebase aborts
        // so the tree never sticks mid-update.
        match run_git(&dir, &["pull", "--ff-only"]) {
            Ok(out) => Ok(push_pull_summary("Updated", &out)),
            Err(_) => apply_or_abort(
                &dir,
                &["pull", "--rebase"],
                &["rebase", "--abort"],
                "Updated (rebased local commits onto the remote)".to_string(),
            ),
        }
    }

    fn unstash(&self) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // Apply + drop the latest stash. An empty stash list or a conflicting pop errors
        // (git leaves the stash in place on conflict); the message surfaces as a Notice.
        run_git(&dir, &["stash", "pop"])?;
        Ok("Popped the latest stash".to_string())
    }

    fn create_patch(&self, commit: &str, path: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // Refuse a MERGE commit up front: `git format-patch -1 <merge>` is misleading (its
        // range spans a parent, so it emits a parent's OWN patch, not the merge's change)
        // and a single mbox patch can't represent a merge for `git am`. `rev-list --parents`
        // prints "<commit> <parent>..."; >2 tokens means more than one parent = a merge.
        let parents = run_git(&dir, &["rev-list", "--parents", "-n", "1", commit])?;
        if parents.split_whitespace().count() > 2 {
            return Err(BackendError(format!("Cannot export a merge commit ({})", short_str(commit))));
        }
        // Refuse an EMPTY commit (no tree change): test the changed-FILE list directly, not a
        // substring scan of the patch - `format-patch` embeds the commit MESSAGE in the body,
        // so a `diff --git` quoted there would fool a text scan. `--root` lets a root commit
        // report its added files (so a real root still exports).
        let changed = run_git(&dir, &["diff-tree", "--root", "--no-commit-id", "--name-only", "-r", commit])?;
        if changed.trim().is_empty() {
            return Err(BackendError(format!("No patch for {} (empty commit)", short_str(commit))));
        }
        // Capture the mbox patch byte-exact (no trim) so the trailing newline survives and
        // the file applies cleanly with `git am`. `-1` = just this commit.
        let patch = run_git_bytes(&dir, &["format-patch", "-1", "--stdout", commit])?;
        // Final backstop independent of git version quirks: never write a zero-byte file
        // behind a reassuring "Wrote" notice if format-patch emitted nothing at all.
        if patch.is_empty() {
            return Err(BackendError(format!("No patch produced for {}", short_str(commit))));
        }
        // The destination is the user's own (editable, modal-confirmed) path - an export is a
        // "Save As" OUTSIDE the repo, so this bypasses the working-tree `safe_join` guard and
        // overwrites by design (re-exporting a commit replaces its file). Deliberately NOT
        // hardened against a /tmp symlink-clobber (O_NOFOLLOW/O_EXCL): this is a single-user
        // desktop TUI where the operator is the sole actor and picks the path themselves.
        std::fs::write(path, &patch).map_err(io_err)?;
        Ok(format!("Wrote {path}"))
    }

    fn working_patch(&self, file: &str) -> Result<String, BackendError> {
        // The clipboard is a TEXT target, so a lossy conversion is acceptable here (a text
        // clipboard cannot carry arbitrary bytes); the file-write path stays byte-exact.
        Ok(String::from_utf8_lossy(&self.working_patch_bytes(file)?).into_owned())
    }

    fn create_working_patch(&self, file: &str, path: &str) -> Result<String, BackendError> {
        let bytes = self.working_patch_bytes(file)?;
        if bytes.is_empty() {
            let what = if file.is_empty() { "No local changes".to_string() } else { format!("No local changes in {file}") };
            return Err(BackendError(what));
        }
        // Same "Save As outside the repo" semantics as create_patch: the user picked the
        // (modal-confirmed) destination, so write through it and overwrite by design. Write
        // the RAW bytes (not a utf8-lossy String) so the file is byte-identical to git's
        // diff and `git apply` accepts it even for non-UTF-8 content.
        std::fs::write(path, &bytes).map_err(io_err)?;
        Ok(format!("Wrote {path}"))
    }

    fn apply_patch(&self, path: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // Apply a unified diff onto the working tree (`git apply`). No `--index`/`--3way`: a plain
        // working-tree apply, like the patches `create_working_patch` writes. A malformed patch or
        // a context mismatch exits nonzero - surfaced as a Notice; the tree is left untouched
        // because `git apply` is atomic (it validates the whole patch before writing).
        run_git(&dir, &["apply", path])?;
        Ok(format!("Applied patch {path}"))
    }

    fn archive_project(&self, rev: &str, path: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // `git archive` writes the archive itself (`-o`) from `rev`'s tree (tracked files at that
        // commit); `WORKING_REV` (the `<current>` row) archives HEAD - `git archive` cannot
        // capture the dirty working tree, so HEAD is the closest committed snapshot. The
        // destination is the user's modal-confirmed path - a "Save As" outside the repo - so it
        // bypasses the working-tree guard and overwrites by design, like create_patch.
        let tree = if rev == WORKING_REV { "HEAD" } else { rev };
        // Pick git's archive format from the destination EXTENSION (git's built-in formats:
        // zip / tar / tar.gz / tgz), so a user-edited extension is honored; anything else
        // defaults to zip. Lets the format picker just seed a sensible default name.
        let lower = path.to_ascii_lowercase();
        let format = if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            "tar.gz"
        } else if lower.ends_with(".tar") {
            "tar"
        } else {
            "zip"
        };
        run_git(&dir, &["archive", &format!("--format={format}"), "-o", path, tree])?;
        Ok(format!("Wrote {path}"))
    }

    fn commit_file(&self, file: &str, message: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // `:(literal)` matches the path verbatim (no glob magic on a `[id].tsx`-style name).
        let spec = literal_pathspec(file);
        // Only an UNTRACKED path must be staged first - `git commit <paths>` otherwise dies with
        // "pathspec did not match". A modified/deleted TRACKED file commits straight through the
        // pathspec, so it is NOT pre-added (a pre-add that survived a later commit failure would
        // leak a staged entry into the index). `--` fences the pathspec.
        let untracked = run_git(&dir, &["ls-files", "--", &spec])?.is_empty();
        if untracked {
            run_git(&dir, &["add", "--", &spec])?;
        }
        // `git commit <paths>` commits ONLY the named path (the implicit `--only`), leaving the
        // rest of the index/working tree as-is. No changes in <file> -> git errors ("nothing to
        // commit"), surfaced as a Notice. Honors the user's hooks/identity like other writes.
        if let Err(e) = run_git(&dir, &["commit", "-m", message, "--", &spec]) {
            // A hook (or "nothing to commit") can reject AFTER we staged a previously-untracked
            // file; unstage it so git's index does not silently disagree with gitgit's
            // no-staging model. Best-effort: the original commit error is what the user sees.
            if untracked {
                let _ = run_git(&dir, &["reset", "--quiet", "--", &spec]);
            }
            return Err(e);
        }
        Ok(format!("Committed {file}"))
    }

    fn commit_dir(&self, dir_path: &str, message: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // `:(literal)` disables glob magic but still matches the directory AND everything
        // under it (a pathspec prefix). Stage every change under the folder - modified, new
        // (untracked), AND deletions (`git add --all` records removals within the pathspec) -
        // so the commit captures the whole subtree, then commit ONLY those paths.
        let spec = literal_pathspec(dir_path);
        run_git(&dir, &["add", "--all", "--", &spec])?;
        // `git commit <paths>` commits only the named subtree (implicit `--only`); a hook or
        // "nothing to commit" leaves the just-staged changes in the index, so reset the
        // pathspec on failure to keep gitgit's no-staging model honest.
        if let Err(e) = run_git(&dir, &["commit", "-m", message, "--", &spec]) {
            let _ = run_git(&dir, &["reset", "--quiet", "--", &spec]);
            return Err(e);
        }
        Ok(format!("Committed {dir_path}/"))
    }

    fn delete_file(&self, file: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // `:(literal)` matches the path verbatim so a glob-magic name (`[id].tsx`) routes to
        // the correct tracked/untracked branch instead of an empty ls-files (which would
        // wrongly fs-remove a tracked file and leave its blob in the index).
        let spec = literal_pathspec(file);
        // A TRACKED file: `git rm -f` removes it from the index AND the working tree (staging
        // the deletion). `-f` is required because plain `git rm` REFUSES a file with local
        // modifications - and Delete is offered on exactly the changed-file case; deleting is
        // destructive by design and already behind a "cannot be undone" confirm. An UNTRACKED
        // file is unknown to git, so just unlink it on disk (through the fenced safe_join).
        if !run_git(&dir, &["ls-files", "--", &spec])?.is_empty() {
            run_git(&dir, &["rm", "-f", "--", &spec])?;
        } else {
            std::fs::remove_file(safe_join(&dir, file)?).map_err(io_err)?;
        }
        Ok(format!("Deleted {file}"))
    }

    fn commit_paths(&self, paths: &[String], message: &str) -> Result<String, BackendError> {
        if paths.is_empty() {
            return Err(BackendError("no files selected to commit".to_string()));
        }
        let dir = self.workdir_path()?;
        // Stage every selected path - modified, new (untracked), AND deletions (`add --all`
        // records removals) - then commit ONLY those paths (implicit `--only`). A literal
        // pathspec per file (no glob magic on a `[id].tsx` name), all fenced after `--`.
        let specs: Vec<String> = paths.iter().map(|p| literal_pathspec(p)).collect();
        let spec_refs: Vec<&str> = specs.iter().map(String::as_str).collect();
        run_git(&dir, &[&["add", "--all", "--"][..], &spec_refs].concat())?;
        let commit = [&["commit", "-m", message, "--"][..], &spec_refs].concat();
        if let Err(e) = run_git(&dir, &commit) {
            // A hook / "nothing to commit" rejects after staging - unstage the selected paths
            // so gitgit's no-staging model stays honest (best-effort; the user sees `e`).
            let _ = run_git(&dir, &[&["reset", "--quiet", "--"][..], &spec_refs].concat());
            return Err(e);
        }
        Ok(format!("Committed {} file(s)", paths.len()))
    }

    fn working_patch_bytes_multi(&self, paths: &[String]) -> Result<Vec<u8>, BackendError> {
        let dir = self.workdir_path()?;
        let specs: Vec<String> = paths.iter().map(|p| literal_pathspec(p)).collect();
        let spec_refs: Vec<&str> = specs.iter().map(String::as_str).collect();
        let pathspec = [&["--"][..], &spec_refs].concat();
        // Same shape as `working_patch_bytes` but over a SET of literal pathspecs: tracked diff
        // vs HEAD (or --cached on an unborn HEAD) plus the untracked sweep over the same set, so
        // a multi-file patch captures every selected file's modified AND brand-new content.
        let has_head = run_git(&dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok();
        let base = if has_head { "HEAD" } else { "--cached" };
        let diff_args = [&["-c", "core.quotePath=false", "diff", base][..], &pathspec].concat();
        let mut out = run_git_bytes(&dir, &diff_args)?;
        let others_args = [&["ls-files", "--others", "--exclude-standard", "-z"][..], &pathspec].concat();
        let others = run_git_bytes(&dir, &others_args)?;
        for seg in others.split(|&b| b == 0).filter(|s| !s.is_empty()) {
            if let Ok(path) = std::str::from_utf8(seg) {
                out.extend(run_git_diff_no_index(&dir, path)?);
            }
        }
        Ok(out)
    }

    fn delete_paths(&self, paths: &[String]) -> Result<String, BackendError> {
        if paths.is_empty() {
            return Err(BackendError("no files selected to delete".to_string()));
        }
        // Per-file delete (each routes tracked->`git rm -f` / untracked->fs unlink like
        // `delete_file`); a failure on one surfaces, the rest already removed.
        for p in paths {
            self.delete_file(p)?;
        }
        Ok(format!("Deleted {} file(s)", paths.len()))
    }

    fn rebase_todo(&self, base: &str, ops: &[(String, String)]) -> Result<String, BackendError> {
        if ops.is_empty() {
            return Err(BackendError("no commits marked for rebase".to_string()));
        }
        let dir = self.workdir_path()?;
        // Refuse a MERGE in any op: its todo verb is `merge`, never `pick`, so the sed would
        // silently no-op (and --rebase-merges rebuilds it under a new hash, defeating the
        // post-check below). The store already keeps merges out of the range; this is the
        // standalone backend guard, mirroring `reword_via_rebase`.
        for (full, _verb) in ops {
            if run_git(&dir, &["rev-parse", "--verify", "--quiet", &format!("{full}^2")]).is_ok() {
                return Err(BackendError(format!("cannot rebase a merge commit ({})", short_str(full))));
            }
        }
        // Build the GIT_SEQUENCE_EDITOR: one `s/^pick <short> /<verb> <short> /` clause per
        // op, matched by git's OWN abbreviation (`rev-parse --short`) so it lines up with the
        // todo (line numbers shift with --rebase-merges label lines). The trailing space
        // anchors the hash so one short hash cannot prefix-match another's line.
        let mut sed = String::from("sed -i");
        for (full, verb) in ops {
            let short = run_git(&dir, &["rev-parse", "--short", full])?;
            sed.push_str(&format!(" -e 's/^pick {short} /{verb} {short} /'"));
        }
        // GIT_EDITOR=true keeps git's auto-combined squash message (and never blocks the
        // loader thread waiting on an editor): a `squash` opens one on the merged message,
        // while `fixup`/`drop` never do. The combined-message-editing UX is a later stage.
        let msg = format!("Rebased {} commit(s)", ops.len());
        run_interactive_rebase(&dir, base, &sed, &[("GIT_EDITOR", "true")], msg.clone(), "rebase")?;
        // Post-check that each op'd commit is gone from HEAD. This RELIABLY catches the
        // degenerate case where NO sed clause matched (e.g. a `merge -C ...` line, never
        // `pick`): the todo is then byte-identical to a plain pick list, git fast-forwards,
        // and every original hash survives as an ancestor. It is only a best-effort backstop
        // for a meld whose sed silently failed WHILE a sibling op's did (then git replays and
        // every hash changes anyway) - but that cannot happen here: all clauses are built the
        // same way from `rev-parse --short` against `abbreviateCommands=false`, so they match
        // or miss together. The squash/fixup TESTS assert the meld by content, not just hash.
        for (full, _verb) in ops {
            if run_git(&dir, &["merge-base", "--is-ancestor", full, "HEAD"]).is_ok() {
                return Err(BackendError(format!("could not rebase {} (still in history)", short_str(full))));
            }
        }
        Ok(msg)
    }

    fn push(&self) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        let out = run_git(&dir, &["push"])?;
        Ok(push_pull_summary("Pushed", &out))
    }

    fn pull(&self) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        let out = run_git(&dir, &["pull", "--ff-only"])?;
        Ok(push_pull_summary("Pulled", &out))
    }

    fn pull_mode(&self, rebase: Option<bool>) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        // None = fast-forward only (refuse a merge); Some(false) = merge; Some(true) = rebase.
        let flag = match rebase {
            None => "--ff-only",
            Some(false) => "--no-rebase",
            Some(true) => "--rebase",
        };
        match run_git(&dir, &["pull", flag]) {
            Ok(out) => Ok(push_pull_summary("Pulled", &out)),
            Err(e) => {
                // A rebase/merge that conflicts leaves a half-done op; abort it so the repo
                // never sticks mid-pull (ff-only never starts one, so nothing to abort).
                let abort: Option<&[&str]> = match rebase {
                    Some(true) => Some(&["rebase", "--abort"]),
                    Some(false) => Some(&["merge", "--abort"]),
                    None => None,
                };
                if let Some(args) = abort {
                    let _ = run_git(&dir, args);
                }
                Err(e)
            }
        }
    }

    fn checkout_ref(&self, name: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(name)?;
        // A branch ATTACHES HEAD; a tag/remote ref DETACHES it - git decides from the ref.
        // Git refuses when the switch would overwrite uncommitted changes (-> Notice).
        run_git(&dir, &["checkout", name])?;
        Ok(format!("Checked out {name}"))
    }

    fn merge_ref(&self, name: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(name)?;
        // `--no-edit` keeps the default merge message (no editor); a conflict aborts so no
        // half-merged tree is left behind.
        apply_or_abort(
            &dir,
            &["merge", "--no-edit", name],
            &["merge", "--abort"],
            format!("Merged {name}"),
        )
    }

    fn rebase_onto(&self, name: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(name)?;
        apply_or_abort(
            &dir,
            &["rebase", name],
            &["rebase", "--abort"],
            format!("Rebased onto {name}"),
        )
    }

    fn branch_rename(&self, old: &str, new: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(new)?;
        run_git(&dir, &["branch", "-m", old, new])?;
        Ok(format!("Renamed {old} to {new}"))
    }

    fn branch_delete(&self, name: &str, force: bool) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(name)?;
        // `-D` force-deletes (skips the merged check); `-d` is the safe delete. Git refuses
        // to delete the branch HEAD is on (-> Notice).
        let flag = if force { "-D" } else { "-d" };
        run_git(&dir, &["branch", flag, name])?;
        Ok(format!("Deleted branch {name}"))
    }

    fn tag_delete(&self, name: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(name)?;
        run_git(&dir, &["tag", "-d", name])?;
        Ok(format!("Deleted tag {name}"))
    }

    fn push_ref(&self, name: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(name)?;
        let remote = default_remote(&dir)?;
        let out = run_git(&dir, &["push", &remote, name])?;
        Ok(push_pull_summary(&format!("Pushed {name}"), &out))
    }

    fn pull_ref(&self, remote: &str, branch: &str, rebase: bool) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(remote)?;
        reject_dashed_ref(branch)?;
        let mode = if rebase { "--rebase" } else { "--no-rebase" };
        match run_git(&dir, &["pull", mode, remote, branch]) {
            Ok(out) => Ok(push_pull_summary("Pulled", &out)),
            Err(e) => {
                // A conflict leaves a half-done rebase/merge; abort the matching op so the
                // repo never sticks mid-pull.
                let abort: &[&str] =
                    if rebase { &["rebase", "--abort"] } else { &["merge", "--abort"] };
                let _ = run_git(&dir, abort);
                Err(e)
            }
        }
    }

    fn remote_list(&self) -> Result<Vec<(String, String)>, BackendError> {
        let repo = self.repo()?;
        let remotes = repo.remotes().map_err(to_err)?;
        let mut out = Vec::with_capacity(remotes.len());
        for i in 0..remotes.len() {
            let Ok(Some(name)) = remotes.get(i) else { continue };
            // A remote with no fetch URL configured (rare) shows a blank URL rather than
            // dropping the row, so it stays editable/removable.
            let url = repo
                .find_remote(name)
                .and_then(|r| r.url().map(str::to_string))
                .unwrap_or_default();
            out.push((name.to_string(), url));
        }
        Ok(out)
    }

    fn remote_add(&self, name: &str, url: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(name)?;
        run_git(&dir, &["remote", "add", name, url])?;
        Ok(format!("Added remote {name}"))
    }

    fn remote_remove(&self, name: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(name)?;
        run_git(&dir, &["remote", "remove", name])?;
        Ok(format!("Removed remote {name}"))
    }

    fn remote_set_url(&self, name: &str, url: &str) -> Result<String, BackendError> {
        let dir = self.workdir_path()?;
        reject_dashed_ref(name)?;
        run_git(&dir, &["remote", "set-url", name, url])?;
        Ok(format!("Set {name} URL"))
    }
}

/// The repo's first configured remote name (`git remote`), falling back to `origin`.
/// Used to push a branch that may have no upstream set yet.
fn default_remote(dir: &std::path::Path) -> Result<String, BackendError> {
    let out = run_git(dir, &["remote"])?;
    Ok(out.lines().next().filter(|l| !l.is_empty()).unwrap_or("origin").to_string())
}

// -- revert (working-tree-only) ---------------------------------------------

/// Undo `commit`'s change to `path` in the working tree, whole-file vs the mainline
/// parent[0] (GoLand semantics). Working-tree ONLY: no index, no commit. Resolves
/// the path in the PARENT tree (root commit -> empty parent): present -> write the
/// parent blob to `<workdir>/path` (Overwritten if the file is also in the commit,
/// else Restored); absent -> remove `<workdir>/path` (Deleted). A bare/no-workdir
/// repo or a workdir-escaping path (absolute / `..` traversal) errors.
fn revert_file_in(repo: &Repository, commit: &str, path: &str) -> Result<RevertOutcome, BackendError> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| BackendError("repository has no working tree (bare)".to_string()))?;
    let dest = safe_join(workdir, path)?;
    let spec = std::path::Path::new(path);

    // The synthetic working row ([`WORKING_REV`]) is not a real commit, so there is no
    // commit-vs-parent to undo: "revert" here means DISCARD the working-tree change, i.e.
    // restore the file to its HEAD content (or remove it if HEAD has no such file - a newly
    // added one). The baseline is HEAD itself, not a mainline parent.
    if commit == WORKING_REV {
        let head_tree = repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .map(|c| c.tree())
            .transpose()
            .map_err(to_err)?;
        return match tree_blob_bytes(repo, head_tree.as_ref(), spec)? {
            Some(bytes) => {
                write_workfile(&dest, &bytes)?;
                Ok(RevertOutcome::Overwritten(path.to_string()))
            }
            None => {
                remove_workfile(&dest)?;
                Ok(RevertOutcome::Deleted(path.to_string()))
            }
        };
    }

    let commit = find_commit(repo, commit)?;
    let parent = mainline_parent(&commit);
    let parent_tree = parent_tree(&parent)?;

    let in_parent = tree_blob_bytes(repo, parent_tree.as_ref(), spec)?;
    let in_commit = tree_blob_bytes(repo, Some(&commit.tree().map_err(to_err)?), spec)?.is_some();

    match in_parent {
        // Present in the parent: write its content back over the working file.
        Some(bytes) => {
            write_workfile(&dest, &bytes)?;
            if in_commit {
                Ok(RevertOutcome::Overwritten(path.to_string()))
            } else {
                Ok(RevertOutcome::Restored(path.to_string()))
            }
        }
        // Absent in the parent: the commit ADDED it -> remove it from the work tree.
        None => {
            remove_workfile(&dest)?;
            Ok(RevertOutcome::Deleted(path.to_string()))
        }
    }
}

/// Revert ONE hunk of `commit`'s change to `path` in the working tree. Builds the
/// REVERSE diff (commit tree -> parent tree) for the file - applying it undoes the
/// commit's change - then applies ONLY the hunk at `hunk_index` to the working
/// directory via a hunk-selecting callback. The reverse diff's hunk ordering matches
/// the forward diff the UI shows, so the index lines up. A hunk that does not apply
/// cleanly (the working file diverged) errors without writing (libgit2 apply is
/// atomic per file). Returns `Overwritten` (a hunk edit always rewrites file content).
fn revert_hunk_in(
    repo: &Repository,
    commit: &str,
    path: &str,
    hunk_index: usize,
) -> Result<RevertOutcome, BackendError> {
    // Reject escaping paths up front (same gate as the whole-file revert).
    let workdir = repo
        .workdir()
        .ok_or_else(|| BackendError("repository has no working tree (bare)".to_string()))?;
    safe_join(workdir, path)?;

    let commit = find_commit(repo, commit)?;
    let parent = mainline_parent(&commit);
    let parent_tree = parent_tree(&parent)?;
    let commit_tree = commit.tree().map_err(to_err)?;

    let mut opts = DiffOptions::new();
    opts.pathspec(path);
    // commit -> parent: the reverse of the commit's change for this file.
    let diff = repo
        .diff_tree_to_tree(Some(&commit_tree), parent_tree.as_ref(), Some(&mut opts))
        .map_err(to_err)?;

    // Cells so the hunk callback (which the ApplyOptions borrows) does not hold a
    // mutable borrow we need to read after apply returns.
    let seen = std::cell::Cell::new(0usize);
    let applied = std::cell::Cell::new(false);
    {
        let mut aopts = ApplyOptions::new();
        aopts.hunk_callback(|_hunk| {
            let i = seen.get();
            seen.set(i + 1);
            let take = i == hunk_index;
            if take {
                applied.set(true);
            }
            take
        });
        repo.apply(&diff, ApplyLocation::WorkDir, Some(&mut aopts))
            .map_err(to_err)?;
    }
    if !applied.get() {
        return Err(BackendError(format!("hunk {hunk_index} not found in {path}")));
    }
    Ok(RevertOutcome::Overwritten(path.to_string()))
}

/// A LITERAL git pathspec for `path`: the `:(literal)` magic prefix tells git to match the
/// string verbatim, disabling glob interpretation of `*`, `?`, `[`, and a leading `:`. Used
/// for the per-file commands so a real filename like `[id].tsx` or `:weird` is targeted
/// exactly, not treated as a pattern (which mis-routes ls-files / errors `add`).
fn literal_pathspec(path: &str) -> String {
    format!(":(literal){path}")
}

/// Join `path` under `workdir`, REJECTING any path that escapes the working tree:
/// an absolute path or one containing a `..`/root component. The single safety gate
/// for the destructive write so a malicious tree entry cannot overwrite outside the
/// repo. Returns the absolute destination path.
fn safe_join(workdir: &std::path::Path, path: &str) -> Result<PathBuf, BackendError> {
    let rel = std::path::Path::new(path);
    use std::path::Component;
    let escapes = rel.components().any(|c| {
        matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_))
    });
    if escapes || rel.as_os_str().is_empty() {
        return Err(BackendError(format!("unsafe revert path: {path}")));
    }
    Ok(workdir.join(rel))
}

/// The raw bytes of the blob at `spec` in `tree`, or `None` when the path is not a
/// blob in that tree (absent, or a directory/submodule). `None` tree -> `None`
/// (the empty parent of a root commit).
fn tree_blob_bytes(
    repo: &Repository,
    tree: Option<&Tree>,
    spec: &std::path::Path,
) -> Result<Option<Vec<u8>>, BackendError> {
    let tree = match tree {
        Some(t) => t,
        None => return Ok(None),
    };
    let entry = match tree.get_path(spec) {
        Ok(e) => e,
        Err(_) => return Ok(None), // path not in this tree
    };
    let object = entry.to_object(repo).map_err(to_err)?;
    match object.as_blob() {
        Some(blob) => Ok(Some(blob.content().to_vec())),
        None => Ok(None), // a tree/submodule, not a file blob
    }
}

/// Write `bytes` to `dest`, creating parent directories. Overwrites any existing
/// file. The single working-tree write of a revert.
fn write_workfile(dest: &std::path::Path, bytes: &[u8]) -> Result<(), BackendError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(io_err)?;
    }
    std::fs::write(dest, bytes).map_err(io_err)
}

/// Remove `dest` from the working tree. A missing file is fine (the revert's intent
/// - the file should not exist - is already satisfied).
fn remove_workfile(dest: &std::path::Path) -> Result<(), BackendError> {
    match std::fs::remove_file(dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(e)),
    }
}

/// Map a filesystem error to our backend error.
fn io_err(e: std::io::Error) -> BackendError {
    BackendError(e.to_string())
}

/// Run the system `git` with `args` in `dir`, returning trimmed stdout on success.
/// A non-zero exit -> `Err` with the first non-empty line of stderr (else stdout) so
/// the status line shows a useful message (e.g. "nothing to commit"). A missing `git`
/// binary -> a clear error. The ONE shell-out point for repo-level writes.
fn run_git(dir: &std::path::Path, args: &[&str]) -> Result<String, BackendError> {
    run_git_env(dir, args, &[])
}

/// Like [`run_git`] but with extra environment variables set on the child (e.g.
/// `GIT_SEQUENCE_EDITOR` / `GIT_EDITOR` to drive a non-interactive rebase reword).
/// The single git invocation + result-parsing site; `run_git` is the no-env case.
fn run_git_env(
    dir: &std::path::Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<String, BackendError> {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let output = cmd
        .output()
        .map_err(|e| BackendError(format!("git not available: {e}")))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    Err(git_failure(&output))
}

/// Run the system `git` with `args` in `dir`, returning RAW (untrimmed) stdout bytes.
/// Unlike [`run_git`], it does NOT trim - the byte-exact output (trailing newline and
/// all) is preserved, which matters for `git format-patch` output a patch tool must
/// apply cleanly. Same failure parsing as [`run_git_env`].
fn run_git_bytes(dir: &std::path::Path, args: &[&str]) -> Result<Vec<u8>, BackendError> {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| BackendError(format!("git not available: {e}")))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(git_failure(&output))
}

/// `git diff --no-index -- /dev/null <file>`: a NEW-file patch for an UNTRACKED path
/// (which `git diff HEAD` omits). `--no-index` follows diff(1) exit codes, so 0 (no diff)
/// and 1 (differs) are both success; only a real error (code > 1, e.g. a missing file)
/// fails. RAW bytes (no trim) so the patch applies byte-exact.
fn run_git_diff_no_index(dir: &std::path::Path, file: &str) -> Result<Vec<u8>, BackendError> {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "core.quotePath=false", "diff", "--no-index", "--", "/dev/null", file])
        .output()
        .map_err(|e| BackendError(format!("git not available: {e}")))?;
    match output.status.code() {
        Some(0) | Some(1) => Ok(output.stdout),
        _ => Err(git_failure(&output)),
    }
}

/// Parse `git blame --porcelain` output into one [`BlameLine`] per source line. The
/// stream emits, per line, a header (`<40-sha> <orig> <final>[ <count>]`), the commit's
/// extended headers (author/author-time/author-tz/...) ONLY on that sha's first
/// appearance, then the `\t`-prefixed source. Author + zoned date are cached per sha so a
/// repeated commit (header-only entry) reuses them. The all-zero sha is an uncommitted
/// working-tree line ("Not Committed Yet"); its short hash renders blank.
fn parse_blame_porcelain(bytes: &[u8], fmt: DateFormat) -> Vec<crate::diff::BlameLine> {
    let text = String::from_utf8_lossy(bytes);
    let mut meta: std::collections::HashMap<String, (String, String)> = std::collections::HashMap::new();
    let mut out = Vec::new();
    let mut sha = String::new();
    let mut author: Option<String> = None;
    let mut atime: Option<i64> = None;
    let mut atz: i32 = 0;
    for line in text.lines() {
        if let Some(content) = line.strip_prefix('\t') {
            // The source line closes an entry: resolve metadata (fresh headers, else cache).
            let (a, d) = match author.take() {
                Some(au) => {
                    let date = format_date(atime.unwrap_or(0), atz, fmt);
                    meta.insert(sha.clone(), (au.clone(), date.clone()));
                    (au, date)
                }
                None => meta.get(&sha).cloned().unwrap_or_default(),
            };
            out.push(crate::diff::BlameLine {
                commit: blame_short_sha(&sha),
                author: a,
                date: d,
                tokens: vec![Token { text: content.to_string(), kind: TokenKind::Ident }],
            });
            atime = None;
            atz = 0;
        } else if is_blame_header(line) {
            sha = line[..40].to_string();
            author = None;
            atime = None;
            atz = 0;
        } else if let Some(a) = line.strip_prefix("author ") {
            author = Some(a.to_string());
        } else if let Some(t) = line.strip_prefix("author-time ") {
            atime = t.trim().parse().ok();
        } else if let Some(z) = line.strip_prefix("author-tz ") {
            atz = parse_blame_tz(z.trim());
        }
    }
    out
}

/// A porcelain line-group header: a 40-hex sha followed by a space (then line numbers).
/// Distinguishes the header from the extended-header keyword lines and the `\t` source.
fn is_blame_header(line: &str) -> bool {
    let b = line.as_bytes();
    b.len() >= 41 && b[40] == b' ' && b[..40].iter().all(u8::is_ascii_hexdigit)
}

/// The 8-char short hash for the blame gutter; the all-zero (uncommitted) sha renders
/// blank so an unsaved line is not mistaken for a real commit.
fn blame_short_sha(sha: &str) -> String {
    if sha.bytes().all(|c| c == b'0') {
        String::new()
    } else {
        sha.chars().take(8).collect()
    }
}

/// Parse a porcelain `author-tz` (`+HHMM` / `-HHMM`) into minutes east of UTC, matching
/// [`format_date`]'s `offset_minutes`. A malformed value falls back to UTC (0).
fn parse_blame_tz(z: &str) -> i32 {
    let sign = if z.starts_with('-') { -1 } else { 1 };
    let digits: Vec<u8> = z.bytes().filter(u8::is_ascii_digit).collect();
    if digits.len() < 4 {
        return 0;
    }
    let h = i32::from(digits[0] - b'0') * 10 + i32::from(digits[1] - b'0');
    let m = i32::from(digits[2] - b'0') * 10 + i32::from(digits[3] - b'0');
    sign * (h * 60 + m)
}

/// Build the error for a failed `git` run: the first non-empty line of stderr (else
/// stdout), so the status line shows a useful message. Shared by the trimmed and raw
/// shell-out helpers.
fn git_failure(output: &std::process::Output) -> BackendError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    BackendError(pick_failure_line(&lines).unwrap_or("git command failed").to_string())
}

/// Pick the line that names the actual failure over git's progress chatter: a merge emits
/// "Auto-merging <f>" before the "CONFLICT ..." line, and a push emits "To <url>" before
/// "error: failed to push ...", so the bare first line reads as success. Choose the first
/// line carrying a failure keyword that is NOT a progress/header line; fall back to the
/// first line. Pure over the collected lines so it is unit-testable without a real `Output`.
fn pick_failure_line<'a>(lines: &[&'a str]) -> Option<&'a str> {
    const KEYS: [&str; 8] =
        ["conflict", "error", "fatal", "failed", "abort", "cannot", "overwritten", "not possible"];
    // Progress/header lines carry a PATH or URL ("Auto-merging src/error.rs", "To x-error.git"),
    // so a keyword in the path/url itself would falsely match and re-mask the real failure line.
    // Skip those prefixes so a conflicting file (or push remote) whose name contains a keyword
    // still surfaces the true error. "to " is the push-destination header (trailing space avoids
    // matching "total ...").
    const PROGRESS: [&str; 6] =
        ["auto-merging", "updating", "removing", "adding", "renaming", "to "];
    lines
        .iter()
        .find(|l| {
            let lo = l.to_lowercase();
            !PROGRESS.iter().any(|p| lo.starts_with(p)) && KEYS.iter().any(|k| lo.contains(k))
        })
        .or_else(|| lines.first())
        .copied()
}

/// The 7-char short prefix of a full hex hash, for status summaries. A byte slice is
/// safe because a hash is ASCII; `.min` guards an already-short input.
fn short_str(commit: &str) -> &str {
    &commit[..commit.len().min(7)]
}

/// Run a conflict-prone history op (`apply`), and on ANY failure run `abort` to roll
/// the repo back so it never sticks mid-operation (cherry-pick / revert). The original
/// git error (the conflict / refusal line) is surfaced; the abort is silent cleanup -
/// a no-op error if nothing was started (e.g. an upfront dirty-tree refusal). Returns
/// `ok_msg` on success. Mirrors how [`reword_via_rebase`] aborts on failure.
fn apply_or_abort(
    dir: &std::path::Path,
    apply: &[&str],
    abort: &[&str],
    ok_msg: String,
) -> Result<String, BackendError> {
    match run_git(dir, apply) {
        Ok(_) => Ok(ok_msg),
        Err(e) => {
            let _ = run_git(dir, abort);
            Err(e)
        }
    }
}

/// Whether `commit` resolves to the same object as HEAD (the fast amend-reword path).
fn is_head_commit(dir: &std::path::Path, commit: &str) -> Result<bool, BackendError> {
    let head = run_git(dir, &["rev-parse", "HEAD"])?;
    let target = run_git(dir, &["rev-parse", commit])?;
    Ok(head == target)
}

/// The full reword message: the edited `subject` (single line) plus `commit`'s existing
/// BODY re-attached, so rewording the subject never drops the message body. An empty
/// body yields just the subject.
fn compose_message(dir: &std::path::Path, commit: &str, subject: &str) -> Result<String, BackendError> {
    let body = run_git(dir, &["log", "-1", "--format=%b", commit])?;
    if body.trim().is_empty() {
        Ok(subject.to_string())
    } else {
        Ok(format!("{subject}\n\n{body}\n"))
    }
}

/// Reword an OLDER commit via a non-interactive `rebase -i`. `--rebase-merges` keeps
/// merge topology (a plain rebase would flatten it); `--autostash` handles a dirty
/// tree. The target's todo line is matched by its SHORT HASH (not a line number, which
/// shifts with `--rebase-merges`/`--root` label lines), and `rebase.abbreviateCommands`
/// is pinned off so the verb is `pick`. ANY failure aborts so the repo never sticks
/// mid-rebase. `msg_path` holds the full message; its shell-quoted path feeds `GIT_EDITOR`.
fn reword_via_rebase(
    dir: &std::path::Path,
    commit: &str,
    msg_path: &std::path::Path,
) -> Result<String, BackendError> {
    // A merge commit's todo verb is `merge`/`label`, never `pick`, so the sed match
    // would be a silent no-op that still exits 0 (a false "Reworded" notice). Refuse it
    // up front rather than report a success that did nothing.
    if run_git(dir, &["rev-parse", "--verify", "--quiet", &format!("{commit}^2")]).is_ok() {
        return Err(BackendError("cannot reword a merge commit".to_string()));
    }
    let short = run_git(dir, &["rev-parse", "--short", commit])?;
    let seq_editor = format!("sed -i -e 's/^pick {short}/reword {short}/'");
    let editor = format!("cp {}", shell_quote(&path_arg(msg_path)));
    // GIT_EDITOR feeds the new message in when the `reword` line stops the rebase.
    run_interactive_rebase(
        dir,
        commit,
        &seq_editor,
        &[("GIT_EDITOR", editor.as_str())],
        "Reworded commit".to_string(),
        "reword",
    )
}

/// Run a non-interactive `git rebase -i` onto `base`'s PARENT (or `--root` when `base` is the
/// root), driving the todo via `seq_editor` (a GIT_SEQUENCE_EDITOR command) plus any
/// `extra_env` (e.g. GIT_EDITOR for reword). Aborts the rebase on ANY failure so the repo
/// never sticks mid-op. The shared scaffold behind [`reword_via_rebase`] + `rebase_todo`.
/// NOTE: callers' `seq_editor` use GNU `sed -i` (the Linux target); a BSD sed would need a
/// backup-suffix arg.
fn run_interactive_rebase(
    dir: &std::path::Path,
    base: &str,
    seq_editor: &str,
    extra_env: &[(&str, &str)],
    ok_msg: String,
    fail_label: &str,
) -> Result<String, BackendError> {
    let mut envs: Vec<(&str, &str)> =
        vec![("GIT_SEQUENCE_EDITOR", seq_editor), ("GIT_TERMINAL_PROMPT", "0")];
    envs.extend_from_slice(extra_env);
    let parent = format!("{base}^");
    let has_parent = run_git(dir, &["rev-parse", "--verify", "--quiet", &parent]).is_ok();
    let onto: &str = if has_parent { &parent } else { "--root" };
    let args = [
        "-c",
        "rebase.abbreviateCommands=false",
        "rebase",
        "-i",
        "--rebase-merges",
        "--autostash",
        onto,
    ];
    match run_git_env(dir, &args, &envs) {
        Ok(_) => Ok(ok_msg),
        Err(e) => {
            let _ = run_git(dir, &["rebase", "--abort"]);
            Err(BackendError(format!("{fail_label} failed, aborted: {}", e.0)))
        }
    }
}

/// A git arg from a path (lossy UTF-8; repo paths here are ASCII temp paths).
fn path_arg(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Reject a ref name that git would parse as an OPTION rather than a name: a leading
/// `-` (e.g. New Branch named `-m` -> `git branch -m <hash>` would RENAME the current
/// branch). git itself rejects the other malformed-name cases, but a positional arg
/// starting with a dash is the dangerous one - guard it at the write boundary.
fn reject_dashed_ref(name: &str) -> Result<(), BackendError> {
    if name.starts_with('-') {
        return Err(BackendError("name cannot start with '-'".to_string()));
    }
    Ok(())
}

/// Single-quote `s` for safe splicing into a shell command (the `GIT_EDITOR` git runs
/// via `sh -c`): wrap in `'...'` and escape any embedded single quote as `'\''`. Defuses
/// spaces / metacharacters in an attacker-or-accident-controlled `TMPDIR` prefix.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Write `message` to a freshly-created temp file (O_EXCL, so a pre-planted symlink
/// cannot redirect the write - CWE-377), with a per-process-unique name, returning its
/// path. The caller shell-quotes the path before splicing it into `GIT_EDITOR`.
fn write_temp_message(commit: &str, message: &str) -> Result<std::path::PathBuf, BackendError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let safe: String = commit.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect();
    let uniq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("gitgit-reword-{safe}-{pid}-{uniq}.txt"));
    // create_new => O_EXCL: fails if the path already exists (defeats a symlink planted
    // at a predictable name). The per-process name makes a benign collision unlikely.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(io_err)?;
    std::io::Write::write_all(&mut f, message.as_bytes()).map_err(io_err)?;
    Ok(path)
}

/// Condense a push/pull's git output into one status line: `<verb>` plus the last
/// non-empty output line (the meaningful summary, e.g. "Already up to date." or the
/// ref update), or just `<verb>` when git was silent.
fn push_pull_summary(verb: &str, out: &str) -> String {
    match out.lines().map(str::trim).rev().find(|l| !l.is_empty()) {
        Some(last) => format!("{verb}: {last}"),
        None => verb.to_string(),
    }
}

// -- commit row + refs ------------------------------------------------------

/// Build a log row from a libgit2 commit: short hash, all parent short hashes
/// (parent[0] is the mainline), author/date, URL-split subject, and decoration.
/// `is_me` is left for `build_repo_model`; `head` is set when this is HEAD.
fn commit_row(
    commit: &Commit,
    refs: &RefIndex,
    head_oid: Option<Oid>,
    fmt: DateFormat,
    now_epoch: i64,
) -> CommitRow {
    let oid = commit.id();
    let author = commit.author();
    let when = author.when();
    let (secs, off) = (when.seconds(), when.offset_minutes());
    CommitRow {
        hash: short_hash(oid),
        full_hash: oid.to_string(),
        parents: commit.parent_ids().map(short_hash).collect(),
        subject: subject_spans(summary(commit)),
        refs: refs.for_oid(oid),
        author: author.name().unwrap_or("").to_string(),
        date: format_when(secs, off, fmt),
        date_label: format_when_relative(secs, off, now_epoch, fmt),
        is_me: false,
        head: head_oid == Some(oid),
        // Cheap fallback so a stale call still renders; the detail panel fills the
        // real containment lazily via `commit_detail`. load_repo must stay cheap,
        // so we do NOT run `git branch --contains` for every row here.
        containing_branches: Vec::new(),
        is_working: false,
        working: None,
    }
}

/// Current Unix time in seconds (UTC), or 0 if the system clock predates the epoch
/// (then relative dates degrade to absolute, never panicking).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A commit's one-line summary, or `""` when it has none / is malformed UTF-8.
/// The returned slice borrows from the commit's owned buffer, hence the explicit
/// tie (clippy's elision hint is wrong: `&Commit<'_>` has two candidate lifetimes).
#[allow(clippy::needless_lifetimes)]
fn summary<'c>(commit: &'c Commit) -> &'c str {
    commit.summary().ok().flatten().unwrap_or("")
}

/// Short hash string (`SHORT_HASH_LEN` hex chars) for an oid.
fn short_hash(oid: Oid) -> String {
    let full = oid.to_string();
    full.chars().take(SHORT_HASH_LEN).collect()
}

/// A map from a commit's short hash to its ref decorations, built once per
/// `load_repo` by scanning every reference (one pass, not a lookup per commit).
struct RefIndex {
    by_hash: std::collections::HashMap<String, Vec<Ref>>,
}

impl RefIndex {
    fn for_oid(&self, oid: Oid) -> Vec<Ref> {
        self.by_hash.get(&short_hash(oid)).cloned().unwrap_or_default()
    }
}

/// Scan all references once, mapping each ref's target commit to its decorations
/// (local/remote branch, tag, HEAD). Tags are peeled to the commit they point at.
fn ref_map(repo: &Repository) -> Result<RefIndex, BackendError> {
    let mut by_hash: std::collections::HashMap<String, Vec<Ref>> = std::collections::HashMap::new();
    let refs = repo.references().map_err(to_err)?;
    for r in refs {
        let r = r.map_err(to_err)?;
        let (kind, name) = match classify_ref(&r) {
            Some(pair) => pair,
            None => continue,
        };
        // Peel to the commit the ref ultimately points at (tags -> their commit).
        let oid = match r.peel_to_commit() {
            Ok(c) => c.id(),
            Err(_) => continue,
        };
        by_hash
            .entry(short_hash(oid))
            .or_default()
            .push(Ref { name, kind });
    }
    Ok(RefIndex { by_hash })
}

/// The branch HEAD points at (None when detached / unborn). Shared by `load_repo` and the
/// `<current>` detail so the working row's branch chip is consistent.
fn head_branch(repo: &Repository) -> Option<String> {
    repo.head()
        .ok()
        .filter(|h| h.is_branch())
        .and_then(|h| h.shorthand().ok().map(str::to_string))
}

/// Classify a reference into our `RefKind` + display name, or `None` to skip it
/// (e.g. a symbolic ref). HEAD is reported as a `Head` decoration.
fn classify_ref(r: &git2::Reference) -> Option<(RefKind, String)> {
    let name = r.shorthand().ok()?.to_string();
    if r.is_branch() {
        Some((RefKind::LocalBranch, name))
    } else if r.is_remote() {
        Some((RefKind::RemoteBranch, name))
    } else if r.is_tag() {
        Some((RefKind::Tag, name))
    } else if r.name().ok() == Some("HEAD") {
        Some((RefKind::Head, name))
    } else {
        None
    }
}

/// The oid HEAD points at, peeled to a commit. `None` for an unborn/empty HEAD.
fn head_oid(repo: &Repository) -> Option<Oid> {
    repo.head().ok()?.peel_to_commit().ok().map(|c| c.id())
}

/// `user.name` from the repo (or global) config; empty string when unset, so the
/// "am I the author" comparison in `build_repo_model` simply never matches.
fn config_user_name(repo: &Repository) -> String {
    repo.config()
        .and_then(|c| c.get_string("user.name"))
        .unwrap_or_default()
}

/// A person+timestamp pair formatted for the detail panel.
fn signature(sig: &git2::Signature, fmt: DateFormat) -> Signature {
    let when = sig.when();
    Signature {
        name: sig.name().unwrap_or("").to_string(),
        email: sig.email().unwrap_or("").to_string(),
        when: format_when(when.seconds(), when.offset_minutes(), fmt),
    }
}

/// Local + remote branches whose tip contains `oid` (the "In N branches" block).
/// EXPENSIVE (a graph walk per branch) - called lazily per commit, never in
/// `load_repo`. A branch contains the commit when its tip IS the commit or is a
/// descendant of it.
fn containing_branches(repo: &Repository, oid: Oid) -> Result<Vec<String>, BackendError> {
    let mut names = Vec::new();
    let branches = repo.branches(None).map_err(to_err)?;
    for branch in branches {
        let (branch, _kind) = branch.map_err(to_err)?;
        let tip = match branch.get().peel_to_commit() {
            Ok(c) => c.id(),
            Err(_) => continue,
        };
        let contains = tip == oid || repo.graph_descendant_of(tip, oid).unwrap_or(false);
        if contains {
            if let Ok(Some(name)) = branch.name() {
                names.push(name.to_string());
            }
        }
    }
    Ok(names)
}

/// Stamp every loaded commit's `containing_branches` with the FULL set of branches
/// whose history reaches it. For each branch, ONE revwalk from its tip marks each
/// loaded commit it encounters (stopping once all loaded commits are accounted for
/// on that branch is not worth the bookkeeping; the cap bounds the walk). This is
/// what the Branch filter selects on, so picking a branch yields its whole reachable
/// history within the loaded window, and the dropdown lists every overlapping branch.
fn stamp_containing_branches(
    repo: &Repository,
    commits: &mut [CommitRow],
) -> Result<(), BackendError> {
    if commits.is_empty() {
        return Ok(());
    }
    // Map short hash -> row index so a walked oid lands on its loaded commit in O(1).
    let index: std::collections::HashMap<String, usize> = commits
        .iter()
        .enumerate()
        .map(|(i, c)| (c.hash.clone(), i))
        .collect();

    for branch in repo.branches(None).map_err(to_err)? {
        let (branch, _kind) = branch.map_err(to_err)?;
        let name = match branch.name() {
            Ok(Some(n)) => n.to_string(),
            _ => continue,
        };
        let tip = match branch.get().peel_to_commit() {
            Ok(c) => c.id(),
            Err(_) => continue,
        };
        mark_branch_ancestry(repo, tip, &name, &index, commits)?;
    }
    Ok(())
}

/// Walk `tip`'s ancestry (newest-first), appending `branch` to each loaded commit
/// the walk reaches. The walk is bounded by the loaded window's reach: history
/// below the oldest loaded commit cannot decorate any loaded row, but a cheap full
/// ancestry walk is acceptable for the capped log. A revwalk failure is non-fatal -
/// the branch simply decorates nothing (the detail panel still recomputes lazily).
fn mark_branch_ancestry(
    repo: &Repository,
    tip: Oid,
    branch: &str,
    index: &std::collections::HashMap<String, usize>,
    commits: &mut [CommitRow],
) -> Result<(), BackendError> {
    let mut walk = match repo.revwalk() {
        Ok(w) => w,
        Err(_) => return Ok(()),
    };
    if walk.push(tip).is_err() {
        return Ok(());
    }
    let mut remaining = index.len();
    for oid in walk {
        if remaining == 0 {
            break; // every loaded commit already decorated by this branch.
        }
        let oid = match oid {
            Ok(o) => o,
            Err(_) => break,
        };
        if let Some(&i) = index.get(&short_hash(oid)) {
            commits[i].containing_branches.push(branch.to_string());
            remaining -= 1;
        }
    }
    Ok(())
}

// -- changed-files tree -----------------------------------------------------

/// Maximum number of blob entries `full_tree` will collect from one commit's
/// tree before refusing. A huge monorepo tree would otherwise build a wall of
/// rows; instead of SILENTLY truncating (which would hide files and mislead a
/// revert), the walk ERRORS once the cap is exceeded so the caller surfaces it.
/// Generous enough for any realistic repository.
const FULL_TREE_CAP: usize = 50_000;

/// Map the synthetic working-tree rev to HEAD for the commit-RESOLVING methods (its
/// diff base IS the HEAD blob); every other hash passes through unchanged. The
/// changed-files + detail paths special-case [`WORKING_REV`] separately (working
/// status, not a commit), so they do NOT go through here.
fn resolve_rev(hash: &str) -> &str {
    if hash == WORKING_REV {
        "HEAD"
    } else {
        hash
    }
}

/// Every uncommitted change in the working tree + index vs HEAD (including untracked
/// files), mapped to our three-state [`FileStatus`]. This is the "<current>" row's
/// file list and the source of its new/changed/deleted counts. Empty when the tree is
/// clean (the working row then reads "no changes" with an empty files pane). Reuses
/// [`map_status`] so the status mapping matches the commit-diff path.
fn working_status_list(repo: &Repository) -> Result<Vec<(String, FileStatus)>, BackendError> {
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let mut opts = DiffOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let diff = repo
        .diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut opts))
        .map_err(to_err)?;

    let mut out = Vec::new();
    for delta in diff.deltas() {
        let status = match map_status(delta.status()) {
            Some(s) => s,
            None => continue,
        };
        let file = match status {
            FileStatus::Deleted => delta.old_file().path(),
            _ => delta.new_file().path(),
        };
        if let Some(p) = file {
            out.push((p.to_string_lossy().into_owned(), status));
        }
    }
    Ok(out)
}

/// Build the synthetic "<current>" log row from the working status: its subject is the
/// compact "+N ~N -N" badge (added / changed / deleted FILE counts, each colored by
/// status like the files pane), its single parent is HEAD (so the graph draws it as a
/// tip above HEAD), and `is_working` marks it for the special-cased render + backend
/// keying. No hash/date columns (blanked at render).
/// Tally working-tree file statuses into `(added, changed, deleted)`. Shared by the
/// `<current>` log badge ([`working_row`]) and its detail-pane summary so they agree.
fn working_counts(working: &[(String, FileStatus)]) -> (usize, usize, usize) {
    let (mut added, mut changed, mut deleted) = (0usize, 0usize, 0usize);
    for (_, st) in working {
        match st {
            FileStatus::Added => added += 1,
            FileStatus::Modified => changed += 1,
            FileStatus::Deleted => deleted += 1,
            FileStatus::Unchanged => {}
        }
    }
    (added, changed, deleted)
}

fn working_row(
    working: &[(String, FileStatus)],
    head_short: Option<String>,
    author: &str,
    current_branch: Option<&str>,
) -> CommitRow {
    let (new, changed, deleted) = working_counts(working);
    // "<current>" colored by working-tree state: DIMMED when clean, accent-blue (Active) when
    // dirty (uncommitted changes). No count badge - the color alone signals the state; the
    // exact per-status counts live in the detail pane. The current branch renders as a ref
    // DECORATION (not baked into the subject) - exactly like a real commit's branch chip - so
    // the local branch's unfilled diamond U+25C7 chip is right-aligned and never truncated.
    let tone = if working.is_empty() { SubjectTone::Dim } else { SubjectTone::Active };
    let subject = vec![SubjectSpan { text: "<current>".to_string(), tone }];
    // The current branch as a local-branch decoration (the unfilled diamond chip), shared
    // with the log's ref-chip renderer so it fits + aligns like every other row's branch.
    let refs = current_branch
        .map(|b| vec![Ref { name: b.to_string(), kind: RefKind::LocalBranch }])
        .unwrap_or_default();
    CommitRow {
        hash: WORKING_REV.to_string(),
        full_hash: String::new(),
        parents: head_short.into_iter().collect(),
        subject,
        refs,
        author: author.to_string(),
        date: String::new(),
        date_label: String::new(),
        is_me: false,
        head: false,
        containing_branches: Vec::new(),
        is_working: true,
        working: Some(crate::model::WorkingSummary {
            branch: current_branch.map(str::to_string),
            added: new,
            changed,
            deleted,
        }),
    }
}

/// The changed-files tree for `hash`: diff its mainline-parent tree against its
/// own tree, map each delta to a `FileStatus`, and fold the flat path list into
/// the collapsed directory tree. Reused by `load_repo` (for the default tree) and
/// the `changed_files` trait method.
fn changed_files_in(repo: &Repository, hash: &str) -> Result<Vec<TreeNode>, BackendError> {
    let commit = find_commit(repo, hash)?;
    let changed = changed_status_list(repo, &commit)?;
    Ok(tree_from_paths(&changed))
}

/// Recursively collect every collapsed path in `nodes` (files AND directories) that
/// the repo's `.gitignore` rules would ignore, into `out`. `prefix` is the accumulated
/// ancestor path (empty at the root, else ending in `/`). Dir paths are keyed by their
/// collapsed name exactly as the files panel reconstructs them, so the per-row lookup
/// agrees. Both files and folders are reported so the All view can dim an ignored
/// directory row as well as its files.
fn collect_ignored(
    repo: &Repository,
    nodes: &[TreeNode],
    prefix: &str,
    out: &mut std::collections::HashSet<String>,
) {
    for node in nodes {
        match node {
            TreeNode::Dir { name, children, .. } => {
                let path = format!("{prefix}{name}");
                if repo.is_path_ignored(&path).unwrap_or(false) {
                    out.insert(path.clone());
                }
                collect_ignored(repo, children, &format!("{path}/"), out);
            }
            TreeNode::File { name, .. } => {
                let path = format!("{prefix}{name}");
                if repo.is_path_ignored(&path).unwrap_or(false) {
                    out.insert(path);
                }
            }
        }
    }
}

/// The FULL file tree of `hash`: every blob in the commit's own tree, with the
/// changed paths carrying their diff-vs-parent[0] status and all other files
/// marked [`FileStatus::Unchanged`]. Reuses `changed_status_list` (the SAME
/// diff-vs-parent logic `changed_files` uses) and `tree_from_paths` (the SAME
/// dir-nesting helper), so neither is duplicated. Errors past [`FULL_TREE_CAP`]
/// blobs rather than silently truncating.
fn full_tree_in(repo: &Repository, hash: &str) -> Result<Vec<TreeNode>, BackendError> {
    let commit = find_commit(repo, hash)?;
    let changed: std::collections::HashMap<String, FileStatus> =
        changed_status_list(repo, &commit)?.into_iter().collect();
    let tree = commit.tree().map_err(to_err)?;

    // Walk the commit tree for every blob path, overlaying the changed status
    // (else Unchanged). Deleted files exist only in the parent, so they are not in
    // this tree's walk; fold them in from the changed map so the All view still
    // shows the deletion (its red, struck-through row) alongside the kept files.
    let mut paths: Vec<(String, FileStatus)> = Vec::new();
    collect_blob_paths(repo, &tree, &mut String::new(), &mut paths, &changed)?;
    for (path, status) in &changed {
        if *status == FileStatus::Deleted {
            paths.push((path.clone(), *status));
        }
    }
    Ok(paths_into_tree(paths))
}

/// The FULL working-tree file tree for the synthetic "<current>" row: HEAD's tree with
/// each path carrying its WORKING status (Modified/Deleted vs HEAD, else Unchanged),
/// plus untracked Added files (and, on an unborn HEAD, everything) folded in. Distinct
/// from [`full_tree_in`], which overlays a COMMIT's own diff-vs-parent - using that for
/// "<current>" wrongly lights the last commit's files as changed and keeps them lit
/// after a commit. A clean tree yields all-Unchanged rows (nothing highlighted).
fn working_full_tree_in(repo: &Repository) -> Result<Vec<TreeNode>, BackendError> {
    let changed: std::collections::HashMap<String, FileStatus> =
        working_status_list(repo)?.into_iter().collect();
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());

    let mut paths: Vec<(String, FileStatus)> = Vec::new();
    if let Some(tree) = &head_tree {
        collect_blob_paths(repo, tree, &mut String::new(), &mut paths, &changed)?;
    }
    // Paths the HEAD-tree walk did not cover (untracked Added; or, on an unborn HEAD,
    // every working file) are appended from the working status so the All view lists them.
    let seen: std::collections::HashSet<&String> = paths.iter().map(|(p, _)| p).collect();
    let extra: Vec<(String, FileStatus)> = changed
        .iter()
        .filter(|(p, _)| !seen.contains(*p))
        .map(|(p, s)| (p.clone(), *s))
        .collect();
    // The same FULL_TREE_CAP the HEAD-tree walk enforces also bounds the appended set, so a
    // near-cap HEAD plus many untracked files (or an unborn HEAD whose whole tree arrives
    // here) cannot silently exceed the All-view bound.
    if paths.len() + extra.len() > FULL_TREE_CAP {
        return Err(BackendError(format!(
            "tree too large for the All view (over {FULL_TREE_CAP} files)"
        )));
    }
    paths.extend(extra);
    Ok(paths_into_tree(paths))
}

/// Fold a (path, status) list into the nested tree via the shared
/// [`tree_from_paths`], deterministically ORDERED so the All view is stable: by
/// path so files group under their directory regardless of walk/delete-append
/// interleaving. (`changed_files_in` keeps git's delta order; only the full tree,
/// which mixes a walk with appended deletes, needs the explicit sort.)
fn paths_into_tree(mut paths: Vec<(String, FileStatus)>) -> Vec<TreeNode> {
    paths.sort_by(|a, b| a.0.cmp(&b.0));
    tree_from_paths(&paths)
}

/// Ordered list of every path this commit CHANGED vs its mainline parent[0] with
/// its `FileStatus`, in git's delta order. The single diff-vs-parent computation
/// shared by `changed_files_in` (folds it straight into a tree, preserving order)
/// and `full_tree_in` (indexes it as an overlay map). A delete keys on the
/// old-side path; everything else the new.
fn changed_status_list(
    repo: &Repository,
    commit: &Commit,
) -> Result<Vec<(String, FileStatus)>, BackendError> {
    let new_tree = commit.tree().map_err(to_err)?;
    let parent = mainline_parent(commit);
    let old_tree = parent_tree(&parent)?;

    let mut opts = DiffOptions::new();
    let diff = repo
        .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut opts))
        .map_err(to_err)?;

    let mut out = Vec::new();
    for delta in diff.deltas() {
        let status = match map_status(delta.status()) {
            Some(s) => s,
            None => continue,
        };
        // A delete reports the path on the old side; everything else on the new.
        let file = match status {
            FileStatus::Deleted => delta.old_file().path(),
            _ => delta.new_file().path(),
        };
        if let Some(p) = file {
            out.push((p.to_string_lossy().into_owned(), status));
        }
    }
    Ok(out)
}

/// Recursively collect every blob path under `tree` into `out`, prefixed by the
/// running `/`-joined `prefix`, tagging each with its changed status from
/// `changed` (else [`FileStatus::Unchanged`]). Bounded by [`FULL_TREE_CAP`]: an
/// over-cap tree ERRORS (no silent truncation). Subtrees recurse; non-blob,
/// non-tree entries (submodules) are skipped.
fn collect_blob_paths(
    repo: &Repository,
    tree: &Tree,
    prefix: &mut String,
    out: &mut Vec<(String, FileStatus)>,
    changed: &std::collections::HashMap<String, FileStatus>,
) -> Result<(), BackendError> {
    for entry in tree.iter() {
        let name = match entry.name() {
            Ok(n) => n,
            Err(_) => continue, // non-UTF-8 name: skip rather than guess.
        };
        match entry.kind() {
            Some(git2::ObjectType::Tree) => {
                let object = entry.to_object(repo).map_err(to_err)?;
                let subtree = match object.as_tree() {
                    Some(t) => t.clone(),
                    None => continue,
                };
                let saved = prefix.len();
                prefix.push_str(name);
                prefix.push('/');
                collect_blob_paths(repo, &subtree, prefix, out, changed)?;
                prefix.truncate(saved);
            }
            Some(git2::ObjectType::Blob) => {
                if out.len() >= FULL_TREE_CAP {
                    return Err(BackendError(format!(
                        "tree too large for the All view (over {FULL_TREE_CAP} files)"
                    )));
                }
                let path = format!("{prefix}{name}");
                let status = changed.get(&path).copied().unwrap_or(FileStatus::Unchanged);
                out.push((path, status));
            }
            _ => {} // submodule / other: not a file the pane shows.
        }
    }
    Ok(())
}

/// The flattened-tree index of the first FILE row (skipping directories), so the
/// startup selection lands on a previewable file. `0` when the tree has no files
/// (all dirs / empty), matching the load_repo fallback.
fn first_file_row(tree: &[TreeNode]) -> usize {
    crate::model::TreeNode::flatten(tree)
        .iter()
        .position(|r| matches!(r.node, crate::model::FlatKind::File { .. }))
        .unwrap_or(0)
}

/// Map a git delta status to our three-state file status, skipping statuses no pane
/// renders (ignored/unmodified/etc.). A rename/copy surfaces as its new path being
/// Added. `Untracked` is a brand-new working-tree file: libgit2's
/// `diff_tree_to_workdir_with_index` (the "<current>" row's working status) reports it
/// as `Untracked`, NOT `Added`, so it is mapped here too - the tree-to-tree callers
/// (commit changesets) never produce `Untracked`, so they are unaffected.
fn map_status(status: Delta) -> Option<FileStatus> {
    match status {
        Delta::Added | Delta::Copied | Delta::Untracked => Some(FileStatus::Added),
        Delta::Deleted => Some(FileStatus::Deleted),
        Delta::Modified | Delta::Renamed | Delta::Typechange => Some(FileStatus::Modified),
        _ => None,
    }
}

// -- preview (diff / source) ------------------------------------------------

/// The git status of `path` between two trees, or `None` if the path is unchanged.
/// Drives `file_view`'s diff-vs-source branch.
fn delta_status(
    repo: &Repository,
    old_tree: Option<&Tree>,
    new_tree: &Tree,
    path: &str,
) -> Result<Option<FileStatus>, BackendError> {
    let mut opts = DiffOptions::new();
    opts.pathspec(path);
    let diff = repo
        .diff_tree_to_tree(old_tree, Some(new_tree), Some(&mut opts))
        .map_err(to_err)?;
    for delta in diff.deltas() {
        if delta_matches_path(&delta, path) {
            return Ok(map_status(delta.status()));
        }
    }
    Ok(None)
}

/// Whether a delta concerns exactly `path` (on either side, for deletes).
fn delta_matches_path(delta: &git2::DiffDelta, path: &str) -> bool {
    let p = std::path::Path::new(path);
    delta.new_file().path() == Some(p) || delta.old_file().path() == Some(p)
}

/// A single-file diff decode: a binary delta (no text body) or the mapped diff lines.
enum PatchBody {
    Binary,
    Lines(Vec<DiffLine>),
}

/// Decode the single-pathspec `diff` (patch index 0 = our file) into diff lines with
/// 1-based old/new numbers + inline-change marks, or `Binary` when libgit2 flags the delta
/// binary (a binary delta emits ZERO body lines). The shared decode behind `diff_view`
/// (tree-vs-tree) and `compare_view` (tree-vs-workdir); the caller owns the rev labels and
/// any empty->source fallback. No delta (nothing changed) yields empty `Lines`. Lines are
/// RAW (a single `Ident` token); the loader highlights later.
fn patch_lines_or_binary(diff: &git2::Diff, line_cap: usize) -> Result<PatchBody, BackendError> {
    // libgit2 flags the delta binary only after it examines content (in Patch::from_diff).
    if let Ok(Some(patch)) = Patch::from_diff(diff, 0) {
        if patch.delta().flags().is_binary() {
            return Ok(PatchBody::Binary);
        }
        let mut lines: Vec<DiffLine> = Vec::new();
        collect_patch_lines(&patch, &mut lines, line_cap)?;
        // Intra-line word highlight: pair each Removed run with the following Added run and
        // mark the chars that differ, so a modified line shows the stronger inline band.
        mark_inline_changes(&mut lines);
        return Ok(PatchBody::Lines(lines));
    }
    Ok(PatchBody::Lines(Vec::new()))
}

/// The Binary preview notice for `path` (a binary delta the viewer cannot show as text).
fn binary_view(path: &str) -> FileView {
    FileView::Binary(crate::diff::BinaryFile {
        path: path.to_string(),
        note: "Binary file differs".to_string(),
    })
}

/// Build a per-file `FileView::Diff` for the tree-vs-tree change of `path` (parent vs
/// commit), labelling the sides with the parent's and commit's short hashes.
fn diff_view(
    repo: &Repository,
    old_tree: Option<&Tree>,
    new_tree: &Tree,
    path: &str,
    commit: &Commit,
    parent: &Option<Commit>,
    line_cap: usize,
) -> Result<FileView, BackendError> {
    // FULL context emits every unchanged line (one whole-file hunk, no gaps) so the diff
    // arrives complete - folding the unchanged middle is the View layer's "Hide unchanged"
    // job (`textdiff::fold_unchanged`), applied uniformly to editable AND read-only diffs.
    // BUT a file longer than `line_cap` would truncate (hiding any change past the cap), so
    // an oversized file falls back to DEFAULT (compact) context, where the changes are always
    // within the emitted window. Normal files (the vast majority) show in full.
    let full = diff_patch_body(repo, old_tree, new_tree, path, line_cap, Some(u32::MAX))?;
    let body = match &full {
        PatchBody::Lines(lines) if lines.len() >= line_cap => {
            diff_patch_body(repo, old_tree, new_tree, path, line_cap, None)?
        }
        _ => full,
    };
    match body {
        PatchBody::Binary => Ok(binary_view(path)),
        PatchBody::Lines(mut lines) => {
            // Full context collapses the file into ONE git hunk (every changed line -> hunk
            // 0), which would make per-hunk revert revert the WHOLE file. Re-stamp each
            // changed line's hunk index from a DEFAULT-context pass - the same grouping
            // `revert_hunk_in`'s reverse diff applies by index - so per-hunk revert stays
            // aligned with what the UI shows. The changed-line SEQUENCE is identical across
            // context widths (context only adds unchanged rows), so the indices line up.
            let seq = changed_hunk_seq(repo, old_tree, new_tree, path)?;
            restamp_changed_hunks(&mut lines, &seq);
            Ok(FileView::Diff(FileDiff {
                path: path.to_string(),
                old_rev: parent.as_ref().map(|p| short_hash(p.id())).unwrap_or_default(),
                new_rev: short_hash(commit.id()),
                lines,
            }))
        }
    }
}

/// Patch body of `path` between two trees at the given `context` (`Some(n)` lines, or
/// `None` for git's default 3). Shared by `diff_view`'s full-context render and its
/// oversized-file compact fallback.
fn diff_patch_body(
    repo: &Repository,
    old_tree: Option<&Tree>,
    new_tree: &Tree,
    path: &str,
    line_cap: usize,
    context: Option<u32>,
) -> Result<PatchBody, BackendError> {
    let mut opts = DiffOptions::new();
    opts.pathspec(path);
    if let Some(c) = context {
        opts.context_lines(c);
    }
    let diff = repo
        .diff_tree_to_tree(old_tree, Some(new_tree), Some(&mut opts))
        .map_err(to_err)?;
    patch_lines_or_binary(&diff, line_cap)
}

/// The hunk index of each CHANGED (`+`/`-`) line, in order, from a DEFAULT-context diff
/// of `path` between two trees - the same hunk grouping `revert_hunk_in` applies by index.
/// Used to re-stamp a full-context display diff so its per-line hunk indices align with the
/// apply-by-index revert. Empty when the path is unchanged.
fn changed_hunk_seq(
    repo: &Repository,
    old_tree: Option<&Tree>,
    new_tree: &Tree,
    path: &str,
) -> Result<Vec<usize>, BackendError> {
    let mut opts = DiffOptions::new();
    opts.pathspec(path);
    let diff = repo
        .diff_tree_to_tree(old_tree, Some(new_tree), Some(&mut opts))
        .map_err(to_err)?;
    let mut seq = Vec::new();
    // The pathspec narrows the diff to this one file, so its patch (if any) is delta 0.
    if let Some(patch) = Patch::from_diff(&diff, 0).map_err(to_err)? {
        for h in 0..patch.num_hunks() {
            let count = patch.num_lines_in_hunk(h).map_err(to_err)?;
            for l in 0..count {
                let line = patch.line_in_hunk(h, l).map_err(to_err)?;
                if matches!(line.origin(), '+' | '-') {
                    seq.push(h);
                }
            }
        }
    }
    Ok(seq)
}

/// Overwrite the `hunk` index of each changed (non-Context) line in `lines` with the
/// matching entry of `seq` (changed lines in order). Context lines keep hunk 0 (the revert
/// scan skips them). A short `seq` leaves any surplus changed line at its current index.
fn restamp_changed_hunks(lines: &mut [DiffLine], seq: &[usize]) {
    let mut k = 0;
    for l in lines.iter_mut() {
        if l.kind != LineKind::Context {
            if let Some(&h) = seq.get(k) {
                l.hunk = h;
            }
            k += 1;
        }
    }
}

/// Walk every hunk line of `patch`, appending mapped `DiffLine`s (capped at
/// `line_cap`). Context/addition/deletion are mapped from the line origin; other
/// origins (file headers, "no newline") are skipped. Between hunks (and before the
/// first one) git omits the unchanged middle; a `fold_marker` row is inserted there
/// labelling the count of hidden lines so the omission is acknowledged instead of a
/// silent line-number jump. The marker count comes from the hunk headers' line
/// numbers, so it needs no extra IO and does not touch hunk indices (per-hunk revert
/// stays correct - the marker is a `Context`/`hunk 0` row the revert scan skips).
fn collect_patch_lines(patch: &Patch, out: &mut Vec<DiffLine>, line_cap: usize) -> Result<(), BackendError> {
    let hunks = patch.num_hunks();
    let mut last_new = 0usize; // highest new-side line number emitted so far (0 = none)
    for h in 0..hunks {
        if out.len() >= line_cap {
            return Ok(());
        }
        // Gap = unchanged lines git omitted before this hunk's first new line. Only emit
        // the marker when a real line can still follow it under the cap (out.len()+1 <
        // line_cap), so a marker is NEVER the final row - the cursor/normalize logic relies
        // on every marker being followed by a browsable line.
        let new_start = patch.hunk(h).map_err(to_err)?.0.new_start() as usize;
        let gap = new_start.saturating_sub(last_new + 1);
        if gap > 0 && out.len() + 1 < line_cap {
            out.push(DiffLine::fold_marker(gap));
        }
        let count = patch.num_lines_in_hunk(h).map_err(to_err)?;
        for l in 0..count {
            if out.len() >= line_cap {
                return Ok(()); // line_cap: stop emitting once the budget is hit.
            }
            let line = patch.line_in_hunk(h, l).map_err(to_err)?;
            if let Some(diff_line) = map_diff_line(&line, h) {
                if let Some(n) = diff_line.new_no {
                    last_new = n;
                }
                out.push(diff_line);
            }
        }
    }
    Ok(())
}

/// Map one libgit2 diff line to a `DiffLine`, or `None` for a non-body origin.
/// `' '` -> Context (both numbers), `'+'` -> Added (new number), `'-'` -> Removed
/// (old number). The raw text becomes a single un-highlighted token.
fn map_diff_line(line: &git2::DiffLine, hunk: usize) -> Option<DiffLine> {
    let (kind, old_no, new_no) = match line.origin() {
        ' ' => (
            LineKind::Context,
            line.old_lineno().map(|n| n as usize),
            line.new_lineno().map(|n| n as usize),
        ),
        '+' => (LineKind::Added, None, line.new_lineno().map(|n| n as usize)),
        '-' => (LineKind::Removed, line.old_lineno().map(|n| n as usize), None),
        _ => return None,
    };
    Some(DiffLine {
        old_no,
        new_no,
        kind,
        tokens: raw_token(&line_text(line)),
        inline_hl: None,
        hunk,
        fold: None,
    })
}

/// Pair each maximal Removed run with the immediately-following Added run and, for
/// each positional (removed, added) pair within it, mark the char span that differs
/// as `inline_hl` on BOTH lines. The span is the gap between the common prefix and
/// common suffix; identical lines (or a 1-sided run) get no mark. Pure; operates on
/// the already-collected lines. The renderer paints `[start, end)` char offsets, so
/// the spans are computed in chars, matching its `code_spans` consumer.
fn mark_inline_changes(lines: &mut [DiffLine]) {
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Removed {
            i += 1;
            continue;
        }
        // [rem_start, rem_end) Removed, then [add_start, add_end) Added.
        let rem_start = i;
        let mut j = i;
        while j < lines.len() && lines[j].kind == LineKind::Removed {
            j += 1;
        }
        let add_start = j;
        let mut k = j;
        while k < lines.len() && lines[k].kind == LineKind::Added {
            k += 1;
        }
        // Position-pair the two runs; unmatched extras (pure add/delete) get nothing.
        let pairs = (j - rem_start).min(k - add_start);
        for p in 0..pairs {
            let old_text = diff_line_text(&lines[rem_start + p]);
            let new_text = diff_line_text(&lines[add_start + p]);
            if let Some((old_span, new_span)) = inline_change_span(&old_text, &new_text) {
                lines[rem_start + p].inline_hl = Some(old_span);
                lines[add_start + p].inline_hl = Some(new_span);
            }
        }
        i = k.max(i + 1);
    }
}

/// The full logical text of a diff line (its single raw token, or "" when empty).
fn diff_line_text(line: &DiffLine) -> String {
    line.tokens.iter().map(|t| t.text.as_str()).collect()
}

/// Char spans `[start, end)` on (old, new) that differ, found by stripping the
/// common char prefix and suffix. `None` when the lines are identical (no inline
/// mark) so identical context-only "changes" are not falsely highlighted.
fn inline_change_span(old: &str, new: &str) -> Option<((usize, usize), (usize, usize))> {
    let o: Vec<char> = old.chars().collect();
    let n: Vec<char> = new.chars().collect();
    if o == n {
        return None;
    }
    let prefix = o.iter().zip(&n).take_while(|(a, b)| a == b).count();
    // Common suffix, not overlapping the matched prefix on either side.
    let max_suffix = (o.len() - prefix).min(n.len() - prefix);
    let suffix = o
        .iter()
        .rev()
        .zip(n.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    Some((
        (prefix, o.len() - suffix),
        (prefix, n.len() - suffix),
    ))
}

/// An unchanged file's source preview: read the blob at `path` in `tree` and split
/// it into raw (un-highlighted) lines. `Ok(None)` when the path is not in the tree
/// or is not UTF-8 text (binary). Lang is derived from the extension for the
/// loader's later highlight pass.
fn source_view(
    repo: &Repository,
    tree: &Tree,
    path: &str,
    line_cap: usize,
) -> Result<Option<FileView>, BackendError> {
    let entry = match tree.get_path(std::path::Path::new(path)) {
        Ok(e) => e,
        Err(_) => return Ok(None), // path does not exist at this commit
    };
    let object = entry.to_object(repo).map_err(to_err)?;
    let blob = match object.as_blob() {
        Some(b) => b,
        None => return Ok(None), // a submodule/tree, not a file
    };
    let text = match std::str::from_utf8(blob.content()) {
        Ok(t) => t,
        Err(_) => return Ok(None), // binary -> no source preview
    };
    let lines: Vec<Vec<Token>> = text
        .lines()
        .take(line_cap) // line_cap: cap very large files.
        .map(raw_token)
        .collect();
    Ok(Some(FileView::Source(SourceFile {
        path: path.to_string(),
        lang: crate::highlight::lang_of(path),
        lines,
    })))
}

/// One raw, un-highlighted token spanning the whole line (empty line -> no
/// tokens). The loader's `highlight_view` replaces these with real tokens.
fn raw_token(text: &str) -> Vec<Token> {
    let text = text.trim_end_matches(['\n', '\r']);
    if text.is_empty() {
        return Vec::new();
    }
    vec![Token {
        text: text.to_string(),
        kind: TokenKind::Ident,
    }]
}

/// Raw text of a diff line as a `String` (libgit2 hands back bytes).
fn line_text(line: &git2::DiffLine) -> String {
    String::from_utf8_lossy(line.content()).into_owned()
}

// -- shared helpers ---------------------------------------------------------

/// Resolve a commit by its (possibly short) hash. Uses libgit2's revparse so a
/// `SHORT_HASH_LEN`-char prefix resolves like `git show <short>`.
fn find_commit<'r>(repo: &'r Repository, hash: &str) -> Result<Commit<'r>, BackendError> {
    let object = repo
        .revparse_single(hash)
        .map_err(|_| BackendError(format!("no such commit: {hash}")))?;
    object
        .peel_to_commit()
        .map_err(|_| BackendError(format!("not a commit: {hash}")))
}

/// The mainline parent (parent[0]) of a commit: the trunk side of a merge, or
/// `None` for a root/parentless commit (which then diffs against an empty tree).
fn mainline_parent<'r>(commit: &Commit<'r>) -> Option<Commit<'r>> {
    commit.parent(0).ok()
}

/// The tree to diff against: the mainline parent's tree, or `None` (an empty tree)
/// for a root commit. `diff_tree_to_tree(None, ...)` treats every file as Added,
/// which is exactly the desired root-commit behavior.
fn parent_tree<'r>(parent: &Option<Commit<'r>>) -> Result<Option<Tree<'r>>, BackendError> {
    match parent {
        Some(p) => Ok(Some(p.tree().map_err(to_err)?)),
        None => Ok(None),
    }
}

/// An empty-repo snapshot: no commits, no tree, the resolved current user.
fn empty_snapshot(current_user: String, status_sig: u64) -> RepoSnapshot {
    RepoSnapshot {
        commits: Vec::new(),
        tree: Vec::new(),
        default_selection: 0,
        current_user,
        unpushed: std::collections::HashSet::new(),
        has_remotes: false,
        truncated: false,
        status_sig,
    }
}

/// Hash (HEAD oid + working-tree statuses) into the change-detection signature. THE
/// single signature function: `load_repo` stamps it into the snapshot and the poll
/// recomputes it over the same inputs - any divergence between the two call sites
/// would make every poll look like an external change.
fn working_sig(head: Option<Oid>, working: &[(String, FileStatus)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    head.map(|o| o.to_string()).hash(&mut hasher);
    let mut entries: Vec<&(String, FileStatus)> = working.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, status) in entries {
        path.hash(&mut hasher);
        (*status as u8).hash(&mut hasher);
    }
    hasher.finish()
}

/// Whether the repo has any remote-tracking ref (`refs/remotes/*`) - the SAME basis
/// `unpushed_hashes` uses to decide "pushed". (A bare remote-tracking ref counts even
/// with no configured remote, matching the solid/hollow graph-node convention, so the
/// reword "already pushed" warning agrees with what the node shows.)
fn has_remote_refs(repo: &Repository) -> bool {
    repo.references_glob("refs/remotes/*")
        .map(|mut refs| refs.any(|r| r.is_ok()))
        .unwrap_or(false)
}

/// Full hashes of loaded commits not reachable from ANY remote-tracking branch
/// (`refs/remotes/*`) - i.e. what `git push` would still send, drawn with a hollow graph
/// node. Empty when the repo has no remote refs (a purely-local repo is not "ahead" of
/// anything, so nothing is flagged). The synthetic working row is skipped (no real hash;
/// the renderer makes it hollow via `is_working`).
fn unpushed_hashes(repo: &Repository, commits: &[CommitRow]) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    let mut walk = match repo.revwalk() {
        Ok(w) => w,
        Err(_) => return HashSet::new(),
    };
    // Newest-first so the `take(cap)` hard-cap drops the OLDEST commits (far below the
    // displayed window), not arbitrary ones - libgit2's default sort order is unspecified.
    let _ = walk.set_sorting(git2::Sort::TIME);
    let mut any_remote = false;
    if let Ok(refs) = repo.references_glob("refs/remotes/*") {
        for r in refs.flatten() {
            // A symbolic remote HEAD has no direct target; only push real tip OIDs.
            if let Some(oid) = r.target() {
                if walk.push(oid).is_ok() {
                    any_remote = true;
                }
            }
        }
    }
    if !any_remote {
        return HashSet::new();
    }
    // We only need to know whether each LOADED commit is reachable from a remote tip, so
    // walk the pushed history but stop as soon as every loaded commit is resolved (the
    // common fully-pushed case ends almost immediately), and hard-cap the walk so a deep
    // pushed history under `--watch` cannot stall the loader. A loaded commit not seen
    // within the cap is treated as unpushed (commits beyond the cap are far older than
    // the displayed window).
    let want: HashSet<git2::Oid> = commits
        .iter()
        .filter(|c| !c.is_working)
        .filter_map(|c| git2::Oid::from_str(&c.full_hash).ok())
        .collect();
    if want.is_empty() {
        return HashSet::new();
    }
    let cap = want.len().saturating_mul(8).max(512);
    let mut pushed: HashSet<git2::Oid> = HashSet::new();
    for oid in walk.flatten().take(cap) {
        if want.contains(&oid) {
            pushed.insert(oid);
            if pushed.len() == want.len() {
                break;
            }
        }
    }
    commits
        .iter()
        .filter(|c| !c.is_working)
        .filter(|c| match git2::Oid::from_str(&c.full_hash) {
            Ok(oid) => !pushed.contains(&oid),
            Err(_) => false,
        })
        .map(|c| c.full_hash.clone())
        .collect()
}

/// Map a libgit2 error to our backend error, carrying its message verbatim.
fn to_err(e: git2::Error) -> BackendError {
    BackendError(e.message().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::build_repo_model;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Unique temp directory per test, removed on drop, so the fs-touching B1
    /// tests stay isolated and deterministic.
    struct TempRepo {
        dir: PathBuf,
    }

    impl TempRepo {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("gitgit-b1-{pid}-{n}"));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempRepo { dir }
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Write `contents` to `name` under the repo dir, creating parent dirs.
    fn write(root: &Path, name: &str, contents: &str) {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// Flatten a tokenized line back to its raw text (blame/source token assertions).
    fn line_text(tokens: &[Token]) -> String {
        tokens.iter().map(|t| t.text.as_str()).collect()
    }

    /// A deterministic signature so dates/authors are stable across runs.
    fn sig() -> git2::Signature<'static> {
        // 2026-05-22 12:08:00 +0000 -> "22.05.2026, 12:08".
        git2::Signature::new("Test Author", "test@example.com", &git2::Time::new(1_779_451_680, 0))
            .unwrap()
    }

    /// Stage everything in the work tree and commit it on top of `parent` (if any),
    /// returning the new commit oid.
    fn commit_all(repo: &Repository, message: &str, parent: Option<Oid>) -> Oid {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let parents: Vec<Commit> = parent
            .map(|p| repo.find_commit(p).unwrap())
            .into_iter()
            .collect();
        let parent_refs: Vec<&Commit> = parents.iter().collect();
        let s = sig();
        repo.commit(Some("HEAD"), &s, &s, message, &tree, &parent_refs)
            .unwrap()
    }

    /// Build a two-commit repo:
    ///   c1 (root): adds a.go, keep.txt, gone.go
    ///   c2:        modifies a.go, adds b.go, deletes gone.go
    /// Returns (backend, short c1, short c2).
    fn build_repo() -> (TempRepo, RealBackend, String, String) {
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();

        write(&tmp.dir, "a.go", "package main\n\nfunc A() {}\n");
        write(&tmp.dir, "keep.txt", "unchanged\n");
        write(&tmp.dir, "src/gone.go", "package src\n");
        let c1 = commit_all(&repo, "root: initial files", None);

        write(&tmp.dir, "a.go", "package main\n\nfunc A() { return }\n");
        write(&tmp.dir, "b.go", "package main\n\nfunc B() {}\n");
        fs::remove_file(tmp.dir.join("src/gone.go")).unwrap();
        let c2 = commit_all(&repo, "second: change, add, delete", Some(c1));

        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();
        (tmp, backend, short_hash(c1), short_hash(c2))
    }

    #[test]
    fn status_sig_tracks_external_changes_and_matches_the_snapshot() {
        let (tmp, backend, _c1, _c2) = build_repo();
        let snap = backend.load_repo().unwrap();
        assert_eq!(
            backend.status_sig().unwrap(),
            snap.status_sig,
            "an idle poll agrees with the loaded snapshot (no spurious refresh)"
        );
        // An external change (a new untracked file) moves the signature...
        write(&tmp.dir, "external.txt", "appeared\n");
        let moved = backend.status_sig().unwrap();
        assert_ne!(moved, snap.status_sig, "a new file changes the signature");
        // ...and the refresh it triggers stamps that SAME signature (the poll and the
        // load share one function - divergence would loop reloads forever).
        assert_eq!(backend.load_repo().unwrap().status_sig, moved);
    }

    #[test]
    fn load_more_grows_the_cap_and_clears_truncation() {
        // A 2-commit repo with the cap at 1: load_repo keeps the newest commit and flags
        // truncated; load_more doubles the cap so the older commit appears and the flag clears.
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        write(&tmp.dir, "a.txt", "one\n");
        let c1 = commit_all(&repo, "first", None);
        write(&tmp.dir, "b.txt", "two\n");
        let _c2 = commit_all(&repo, "second", Some(c1));
        let mut config = Config::default();
        config.behavior.commit_cap = 1;
        let backend = RealBackend::open(&tmp.dir, &config).unwrap();

        let first = backend.load_repo().unwrap();
        assert!(first.truncated, "more history exists beyond the cap");
        // commits = [<current>, newest real commit] = 2 rows (one real commit under the cap).
        let real_first = first.commits.iter().filter(|c| !c.is_working).count();
        assert_eq!(real_first, 1, "only the newest commit fit under cap=1");

        let more = backend.load_more().unwrap();
        assert!(!more.truncated, "both commits now fit -> no more history");
        let real_more = more.commits.iter().filter(|c| !c.is_working).count();
        assert_eq!(real_more, 2, "load_more revealed the older commit");
    }

    #[test]
    fn open_file_is_editable_only_for_current_else_read_only_parent_vs_commit() {
        use crate::backend::OpenFile;
        use crate::diff::FileView;
        let (_tmp, backend, c1, c2) = build_repo();

        // The <current> working row is EDITABLE: base = HEAD blob, work = the worktree.
        match backend.open_file(WORKING_REV, "a.go").unwrap() {
            OpenFile::Editable { .. } => {}
            OpenFile::ReadOnly(_) => panic!("the <current> row must open an editable buffer"),
        }

        // A historical commit is READ-ONLY and shows PARENT-vs-COMMIT: the right side is
        // the selected commit's blob (new_rev == c2), the left its parent's (old_rev == c1).
        match backend.open_file(&c2, "a.go").unwrap() {
            OpenFile::ReadOnly(Some(FileView::Diff(d))) => {
                assert_eq!(d.new_rev, c2, "right side = the selected commit");
                assert_eq!(d.old_rev, c1, "left side = its parent");
            }
            other => panic!("a historical commit must be a read-only parent-vs-commit diff: {other:?}"),
        }
    }

    #[test]
    fn b1_load_repo_commits_parents_author_date_refs() {
        let (_tmp, backend, c1, c2) = build_repo();
        let snap = backend.load_repo().unwrap();

        // A clean tree still pins the "<current>" row at index 0, then the two real
        // commits follow (newest-first). The clean row is just the DIMMED "<current>"
        // label - no count badge, no "no changes" note (the color signals the state).
        assert_eq!(snap.commits.len(), 3, "the <current> row plus two commits walked");
        assert!(snap.commits[0].is_working, "row 0 is the pinned <current> row");
        assert_eq!(snap.commits[0].subject.len(), 1, "one subject span, no badge");
        assert_eq!(snap.commits[0].subject[0].text, "<current>");
        assert_eq!(
            snap.commits[0].subject[0].tone,
            crate::model::SubjectTone::Dim,
            "a clean <current> is dimmed"
        );
        let ws = snap.commits[0].working.as_ref().expect("working summary");
        assert_eq!((ws.added, ws.changed, ws.deleted), (0, 0, 0), "clean tree has zero counts");
        // HEAD (c2) is row 1, its mainline parent is c1.
        assert_eq!(snap.commits[1].hash, c2);
        assert_eq!(snap.commits[1].parents, vec![c1.clone()], "parent linkage");
        assert!(snap.commits[2].parents.is_empty(), "root has no parents");
        // Author + date populated from the signature.
        assert_eq!(snap.commits[1].author, "Test Author");
        assert_eq!(snap.commits[1].date, "22.05.2026, 12:08");
        assert!(snap.commits[1].head, "row 1 is HEAD");
        // The default branch decorates its tip (git init -> master or main).
        let default_branch = snap.commits[1]
            .refs
            .iter()
            .any(|r| r.kind == RefKind::LocalBranch);
        assert!(default_branch, "the default branch decorates HEAD");

        // build_repo_model stamps is_me from the (empty) current_user without panic.
        let model = build_repo_model(snap);
        assert_eq!(model.graph.rows.len(), 3);
    }

    #[test]
    fn b1_changed_files_statuses() {
        let (_tmp, backend, _c1, c2) = build_repo();
        let tree = backend.changed_files(&c2).unwrap();
        let mut by_status: Vec<(String, FileStatus)> = Vec::new();
        collect_files(&tree, &mut by_status);

        let status_of = |name: &str| by_status.iter().find(|(n, _)| n == name).map(|(_, s)| *s);
        assert_eq!(status_of("a.go"), Some(FileStatus::Modified), "a.go modified");
        assert_eq!(status_of("b.go"), Some(FileStatus::Added), "b.go added");
        assert_eq!(status_of("gone.go"), Some(FileStatus::Deleted), "gone.go deleted");
        assert_eq!(by_status.len(), 3, "exactly the three changed files");
    }

    #[test]
    fn b1_working_row_includes_untracked_and_counts() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // Dirty the tree vs HEAD: modify a tracked file + add a brand-new untracked one.
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { /* edited */ }\n");
        write(&tmp.dir, "fresh.txt", "brand new\n");

        // The "<current>" working row is pinned at index 0. Its per-status counts INCLUDE
        // the untracked file (libgit2 reports it as Delta::Untracked) and live in the
        // working summary; the subject is just the accent-blue "<current>" label (no badge).
        let snap = backend.load_repo().unwrap();
        let row = &snap.commits[0];
        assert!(row.is_working, "a dirty tree prepends the <current> row");
        assert_eq!(row.subject.len(), 1, "one subject span, no badge");
        assert_eq!(row.subject[0].text, "<current>");
        assert_eq!(
            row.subject[0].tone,
            crate::model::SubjectTone::Active,
            "a dirty <current> is accent-blue"
        );
        let ws = row.working.as_ref().expect("working summary");
        assert_eq!(ws.added, 1, "untracked counted as added");
        assert_eq!(ws.changed, 1, "modified counted as changed");

        // The working tree lists the untracked file as an Added row.
        let working = backend.changed_files(WORKING_REV).unwrap();
        let mut files: Vec<(String, FileStatus)> = Vec::new();
        collect_files(&working, &mut files);
        assert_eq!(
            files.iter().find(|(n, _)| n == "fresh.txt").map(|(_, s)| *s),
            Some(FileStatus::Added),
            "the untracked file appears as Added: {files:?}"
        );
    }

    #[test]
    fn b1_full_tree_overlays_changed_status_on_every_file() {
        let (_tmp, backend, _c1, c2) = build_repo();
        // c2's FULL tree: a.go (Modified), b.go (Added), keep.txt (Unchanged), and
        // the deleted src/gone.go folded back in as Deleted. Every blob present.
        let tree = backend.full_tree(&c2).unwrap();
        let mut files: Vec<(String, FileStatus)> = Vec::new();
        collect_files(&tree, &mut files);
        let status_of = |name: &str| files.iter().find(|(n, _)| n == name).map(|(_, s)| *s);

        assert_eq!(status_of("a.go"), Some(FileStatus::Modified), "a.go carries its Modified status");
        assert_eq!(status_of("b.go"), Some(FileStatus::Added), "b.go carries its Added status");
        assert_eq!(status_of("gone.go"), Some(FileStatus::Deleted), "the deleted file folds back in");
        assert_eq!(
            status_of("keep.txt"),
            Some(FileStatus::Unchanged),
            "an untouched file is Unchanged (plain)"
        );
        // The full tree shows MORE than the changed-only tree (it adds keep.txt).
        let changed = backend.changed_files(&c2).unwrap();
        let mut changed_files: Vec<(String, FileStatus)> = Vec::new();
        collect_files(&changed, &mut changed_files);
        assert!(
            files.len() > changed_files.len(),
            "the full tree has every file, the changed tree only the diff"
        );
        assert!(
            files.iter().any(|(n, _)| n == "keep.txt"),
            "the unchanged keep.txt appears only in the full tree"
        );
        assert!(
            !changed_files.iter().any(|(n, _)| n == "keep.txt"),
            "keep.txt is NOT in the changed-only tree"
        );
    }

    #[test]
    fn working_full_tree_overlays_working_status_not_the_last_commits_changes() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // Dirty the working tree: edit a tracked file, drop an untracked one, delete a
        // tracked one. b.go was ADDED in HEAD (c2) but is UNCHANGED vs the working tree -
        // the regression lit it.
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { return 1 }\n");
        write(&tmp.dir, "new.go", "package main\n");
        fs::remove_file(tmp.dir.join("keep.txt")).unwrap();

        let tree = backend.full_tree(WORKING_REV).unwrap();
        let mut files: Vec<(String, FileStatus)> = Vec::new();
        collect_files(&tree, &mut files);
        let status_of = |name: &str| files.iter().find(|(n, _)| n == name).map(|(_, s)| *s);

        assert_eq!(status_of("a.go"), Some(FileStatus::Modified), "the working edit is Modified");
        assert_eq!(status_of("new.go"), Some(FileStatus::Added), "the untracked file is Added");
        assert_eq!(
            status_of("keep.txt"),
            Some(FileStatus::Deleted),
            "a deleted tracked file appears once as Deleted (not double-counted)"
        );
        assert_eq!(
            files.iter().filter(|(n, _)| n == "keep.txt").count(),
            1,
            "the deleted file is not duplicated by the untracked-append pass"
        );
        assert_eq!(
            status_of("b.go"),
            Some(FileStatus::Unchanged),
            "a file the LAST COMMIT added is Unchanged vs the working tree (not lit)"
        );
    }

    #[test]
    fn working_full_tree_is_all_unchanged_when_clean() {
        // After committing everything (a clean tree), the <current> All view must light
        // NOTHING - the post-commit-stale regression.
        let (_tmp, backend, _c1, _c2) = build_repo();
        let tree = backend.full_tree(WORKING_REV).unwrap();
        let mut files: Vec<(String, FileStatus)> = Vec::new();
        collect_files(&tree, &mut files);
        assert!(!files.is_empty(), "the clean working tree still lists its files");
        assert!(
            files.iter().all(|(_, s)| *s == FileStatus::Unchanged),
            "a clean tree highlights no file as changed"
        );
    }

    #[test]
    fn b1_ignored_paths_reports_force_added_files_and_nested_dirs() {
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        // A .gitignore covering a file glob + a whole nested directory.
        write(&tmp.dir, ".gitignore", "*.log\nsecret/\n");
        write(&tmp.dir, "a.go", "package main\n");
        write(&tmp.dir, "debug.log", "noise\n");
        write(&tmp.dir, "secret/key.txt", "shh\n");
        write(&tmp.dir, "secret/nested/deep.txt", "deeper\n");
        // FORCE-add so the ignored files land in the commit (mirrors a force-add); the
        // tree therefore HOLDS files the ignore rules would otherwise exclude.
        let mut index = repo.index().unwrap();
        index.add_all(["*"].iter(), git2::IndexAddOption::FORCE, None).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let s = sig();
        repo.commit(Some("HEAD"), &s, &s, "force-add ignored files", &tree, &[]).unwrap();

        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();
        let ignored = backend.ignored_paths("HEAD").unwrap();
        assert!(ignored.contains("debug.log"), "a *.log match is ignored: {ignored:?}");
        assert!(ignored.contains("secret/key.txt"), "a file under an ignored dir is ignored");
        assert!(ignored.contains("secret/nested/deep.txt"), "a NESTED file under an ignored dir is ignored");
        // The ignored DIRECTORY itself (and the nested subdir) are reported, so the All
        // view can dim the folder rows too.
        assert!(ignored.contains("secret"), "the ignored directory row is reported: {ignored:?}");
        assert!(ignored.contains("secret/nested"), "the nested ignored subdir is reported");
        assert!(!ignored.contains("a.go"), "a normal tracked file is NOT ignored");
        assert!(!ignored.contains(".gitignore"), "the .gitignore itself is not ignored");
    }

    #[test]
    fn b1_unpushed_flags_commits_not_on_a_remote_tracking_branch() {
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        let s = sig();
        // Two linear commits, c1 then c2 (HEAD).
        write(&tmp.dir, "a.txt", "1\n");
        let mut index = repo.index().unwrap();
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree1 = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let c1 = repo.commit(Some("HEAD"), &s, &s, "c1", &tree1, &[]).unwrap();
        write(&tmp.dir, "a.txt", "2\n");
        let mut index = repo.index().unwrap();
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree2 = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.find_commit(c1).unwrap();
        let c2 = repo.commit(Some("HEAD"), &s, &s, "c2", &tree2, &[&parent]).unwrap();

        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();
        // No remote at all -> nothing is "ahead", so nothing is flagged.
        assert!(
            backend.load_repo().unwrap().unpushed.is_empty(),
            "a purely-local repo flags nothing unpushed"
        );

        // Pretend c1 was pushed: a remote-tracking ref at c1. c2 is ahead of it.
        repo.reference("refs/remotes/origin/master", c1, true, "track").unwrap();
        let unpushed = backend.load_repo().unwrap().unpushed;
        assert!(
            unpushed.contains(&c2.to_string()),
            "c2 is ahead of the remote tip -> unpushed: {unpushed:?}"
        );
        assert!(!unpushed.contains(&c1.to_string()), "c1 is on the remote -> pushed");
    }

    #[test]
    fn b1_current_branch_chip_is_the_local_unfilled_diamond() {
        let (_tmp, backend, _c1, _c2) = build_repo();
        let repo = Repository::open(&_tmp.dir).unwrap();
        let branch = repo.head().unwrap().shorthand().unwrap().to_string();

        // The <current> row's branch is a local-branch DECORATION (ref), rendered by the
        // shared ref-chip path as the unfilled diamond U+25C7 - NOT baked into the subject -
        // so it fits + aligns like every other row's branch chip.
        let snap = backend.load_repo().unwrap();
        assert!(snap.commits[0].is_working, "row 0 is the <current> row");
        let branch_ref = snap.commits[0].refs.iter().find(|r| r.name == branch).unwrap();
        assert_eq!(branch_ref.kind, RefKind::LocalBranch, "the <current> branch is a local-branch ref");

        // Publishing to origin adds a SEPARATE remote-tracking decoration (classified
        // RemoteBranch -> the render layer draws it with the FILLED diamond). The <current>
        // row keeps its own local-branch chip regardless.
        let head_oid = repo.head().unwrap().target().unwrap();
        repo.reference(&format!("refs/remotes/origin/{branch}"), head_oid, true, "track").unwrap();
        let snap = backend.load_repo().unwrap();
        assert_eq!(
            snap.commits[0].refs.iter().find(|r| r.name == branch).map(|r| r.kind),
            Some(RefKind::LocalBranch),
            "the <current> chip stays a local branch even after origin tracks it",
        );
        let remote_ref = snap.commits.iter().flat_map(|c| &c.refs).find(|r| r.kind == RefKind::RemoteBranch);
        assert!(remote_ref.is_some(), "origin/<branch> is decorated as a remote branch");
    }

    #[test]
    fn b1_full_tree_root_commit_marks_all_added() {
        let (_tmp, backend, c1, _c2) = build_repo();
        // The root commit's full tree: every file is Added vs the empty parent (none
        // Unchanged - there is no parent for anything to be unchanged against).
        let tree = backend.full_tree(&c1).unwrap();
        let mut files: Vec<(String, FileStatus)> = Vec::new();
        collect_files(&tree, &mut files);
        assert!(!files.is_empty(), "the root full tree lists its files");
        assert!(
            files.iter().all(|(_, s)| *s == FileStatus::Added),
            "every root-commit file is Added (no Unchanged at the root)"
        );
    }

    #[test]
    fn b1_full_tree_unchanged_file_previews_as_source() {
        let (_tmp, backend, _c1, c2) = build_repo();
        // An Unchanged file in the All view must preview as the FULL FILE (Source),
        // not a diff: keep.txt is untouched by c2, so file_view returns its blob.
        let tree = backend.full_tree(&c2).unwrap();
        let mut files: Vec<(String, FileStatus)> = Vec::new();
        collect_files(&tree, &mut files);
        assert_eq!(
            files.iter().find(|(n, _)| n == "keep.txt").map(|(_, s)| *s),
            Some(FileStatus::Unchanged),
            "keep.txt is Unchanged in the full tree"
        );
        let view = backend.file_view(&c2, "keep.txt").unwrap().unwrap();
        let FileView::Source(source) = view else {
            panic!("an Unchanged file previews as Source (the full file), not a Diff");
        };
        assert_eq!(source.path, "keep.txt");
        assert_eq!(source.lines.len(), 1, "the full blob's single line is shown");
    }

    #[test]
    fn b1_root_commit_changed_files_vs_empty_tree() {
        let (_tmp, backend, c1, _c2) = build_repo();
        // The root commit diffs against an EMPTY tree -> every file is Added.
        let tree = backend.changed_files(&c1).unwrap();
        let mut files: Vec<(String, FileStatus)> = Vec::new();
        collect_files(&tree, &mut files);
        assert!(!files.is_empty(), "root commit reports its files");
        assert!(
            files.iter().all(|(_, s)| *s == FileStatus::Added),
            "all root-commit files are Added (vs empty tree)"
        );
    }

    #[test]
    fn b1_commit_detail_containing_branches() {
        let (_tmp, backend, c1, c2) = build_repo();
        let detail = backend.commit_detail(&c2).unwrap();
        assert_eq!(detail.short_hash, c2);
        assert_eq!(detail.committer.name, "Test Author");
        assert_eq!(detail.author.email, "test@example.com");
        assert!(
            !detail.containing_branches.is_empty(),
            "the default branch contains HEAD"
        );
        // The default branch (tip = c2) is a descendant of the root c1, so it
        // contains c1 too.
        let root_detail = backend.commit_detail(&c1).unwrap();
        assert!(
            !root_detail.containing_branches.is_empty(),
            "the branch tip is a descendant of the root commit"
        );
    }

    #[test]
    fn inline_change_span_marks_only_the_differing_middle() {
        // Common prefix "let x = " and common suffix ";" -> only the middle differs.
        let (old, new) = inline_change_span("let x = 1;", "let x = 22;").unwrap();
        assert_eq!(old, (8, 9), "old: the '1' between prefix and suffix");
        assert_eq!(new, (8, 10), "new: the '22' between prefix and suffix");
        // Identical lines carry no inline mark.
        assert!(inline_change_span("same", "same").is_none());
        // A pure suffix change (no shared trailing run) spans to end on both sides.
        let (o, n) = inline_change_span("abc", "abXY").unwrap();
        assert_eq!(o, (2, 3));
        assert_eq!(n, (2, 4));
    }

    #[test]
    fn mark_inline_changes_pairs_removed_then_added_runs() {
        // A Removed line followed by an Added line gets paired and both marked.
        let mut lines = vec![
            DiffLine {
                old_no: Some(1),
                new_no: None,
                kind: LineKind::Removed,
                tokens: raw_token("let x = 1;"),
                inline_hl: None,
                hunk: 0,
                fold: None,
            },
            DiffLine {
                old_no: None,
                new_no: Some(1),
                kind: LineKind::Added,
                tokens: raw_token("let x = 2;"),
                inline_hl: None,
                hunk: 0,
                fold: None,
            },
        ];
        mark_inline_changes(&mut lines);
        assert_eq!(lines[0].inline_hl, Some((8, 9)), "removed line marks the changed char");
        assert_eq!(lines[1].inline_hl, Some((8, 9)), "added line marks the changed char");
    }

    #[test]
    fn b1_load_repo_stamps_branch_membership_on_buried_commits() {
        // The Branch filter reads `containing_branches`, stamped at load for EVERY
        // loaded commit - not just the branch tip - so a buried commit carries the
        // branch whose history reaches it. The default branch (tip = c2) reaches the
        // root c1, so c1's row must carry it (the by-tip-ref filter could not).
        let (_tmp, backend, _c1, c2) = build_repo();
        let snap = backend.load_repo().unwrap();
        let root = snap.commits.iter().find(|c| !c.is_working && c.hash != c2).unwrap();
        assert!(
            !root.containing_branches.is_empty(),
            "the buried root commit is stamped with the branch that reaches it"
        );
        // The tip carries the same branch (membership includes the tip itself).
        let tip = snap.commits.iter().find(|c| c.hash == c2).unwrap();
        assert_eq!(
            tip.containing_branches, root.containing_branches,
            "both commits on the single default branch share its membership"
        );
    }

    #[test]
    fn b1_file_view_diff_and_source_and_missing() {
        let (_tmp, backend, _c1, c2) = build_repo();

        // a.go changed in c2 -> a Diff with +/- line numbers.
        let view = backend.file_view(&c2, "a.go").unwrap().unwrap();
        let FileView::Diff(diff) = view else {
            panic!("changed file -> Diff");
        };
        assert_eq!(diff.new_rev, c2);
        let has_added = diff
            .lines
            .iter()
            .any(|l| l.kind == LineKind::Added && l.new_no.is_some() && l.old_no.is_none());
        let has_removed = diff
            .lines
            .iter()
            .any(|l| l.kind == LineKind::Removed && l.old_no.is_some() && l.new_no.is_none());
        assert!(has_added && has_removed, "diff carries +/- lines with line numbers");

        // keep.txt unchanged in c2 -> a Source view of the blob.
        let src = backend.file_view(&c2, "keep.txt").unwrap().unwrap();
        let FileView::Source(source) = src else {
            panic!("unchanged file -> Source");
        };
        assert_eq!(source.lines.len(), 1, "keep.txt has one source line");

        // A path absent at this commit -> None.
        assert!(backend.file_view(&c2, "nope.go").unwrap().is_none());
    }

    /// RealBackend must stay `Send + Sync` (it holds only a `PathBuf`), so a later
    /// loader can drive it off the UI thread. A compile-time assertion.
    #[test]
    fn b1_real_backend_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RealBackend>();
    }

    // -- revert (working-tree-only) -----------------------------------------
    //
    // build_repo() leaves the work tree at c2: a.go modified, b.go added,
    // src/gone.go deleted (vs c1). Reverting c2's change to a file undoes that
    // change in the WORK TREE only (no index, no commit), whole-file vs c1.

    #[test]
    fn b1_revert_modified_restores_parent_bytes() {
        let (tmp, backend, _c1, c2) = build_repo();
        // a.go was MODIFIED in c2 -> overwrite the work file with c1's bytes.
        let outcome = backend.revert_file(&c2, "a.go").unwrap();
        assert!(matches!(outcome, RevertOutcome::Overwritten(ref p) if p == "a.go"));
        let on_disk = fs::read_to_string(tmp.dir.join("a.go")).unwrap();
        assert_eq!(on_disk, "package main\n\nfunc A() {}\n", "work file == parent[0] blob");
    }

    #[test]
    fn b1_revert_added_removes_from_work_tree() {
        let (tmp, backend, _c1, c2) = build_repo();
        assert!(tmp.dir.join("b.go").exists(), "b.go present before revert");
        // b.go was ADDED in c2 -> absent in parent -> delete it.
        let outcome = backend.revert_file(&c2, "b.go").unwrap();
        assert!(matches!(outcome, RevertOutcome::Deleted(ref p) if p == "b.go"));
        assert!(!tmp.dir.join("b.go").exists(), "added file removed from work tree");
    }

    #[test]
    fn b1_revert_deleted_restores_parent_blob() {
        let (tmp, backend, _c1, c2) = build_repo();
        assert!(!tmp.dir.join("src/gone.go").exists(), "gone.go absent before revert");
        // src/gone.go was DELETED in c2 -> present in parent -> restore it.
        let outcome = backend.revert_file(&c2, "src/gone.go").unwrap();
        assert!(matches!(outcome, RevertOutcome::Restored(ref p) if p == "src/gone.go"));
        let on_disk = fs::read_to_string(tmp.dir.join("src/gone.go")).unwrap();
        assert_eq!(on_disk, "package src\n", "deleted file restored from parent blob");
    }

    #[test]
    fn b1_revert_root_added_removes_file() {
        let (tmp, backend, c1, _c2) = build_repo();
        // c1 is the ROOT: a.go is "added" vs the empty parent -> reverting deletes it.
        assert!(tmp.dir.join("a.go").exists(), "a.go present before root revert");
        let outcome = backend.revert_file(&c1, "a.go").unwrap();
        assert!(matches!(outcome, RevertOutcome::Deleted(ref p) if p == "a.go"));
        assert!(!tmp.dir.join("a.go").exists(), "root-added file removed (empty parent)");
    }

    #[test]
    fn revert_working_row_restores_head_and_drops_new_files() {
        // The synthetic <current> row is not a real commit: reverting a file there DISCARDS
        // the working change (restore HEAD content), and reverting a NEW (untracked) file
        // removes it. Without the WORKING_REV special case this errored ("no such commit").
        let (tmp, backend, _c1, _c2) = build_repo();
        // a.go is tracked at HEAD; dirty it in the work tree, then revert the working row.
        let head_bytes = fs::read_to_string(tmp.dir.join("a.go")).unwrap();
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { /* local edit */ }\n");
        let outcome = backend.revert_file(WORKING_REV, "a.go").unwrap();
        assert!(matches!(outcome, RevertOutcome::Overwritten(ref p) if p == "a.go"));
        assert_eq!(fs::read_to_string(tmp.dir.join("a.go")).unwrap(), head_bytes, "restored to HEAD content");

        // A brand-new untracked file is not in HEAD -> reverting removes it.
        write(&tmp.dir, "fresh.txt", "scratch\n");
        let outcome = backend.revert_file(WORKING_REV, "fresh.txt").unwrap();
        assert!(matches!(outcome, RevertOutcome::Deleted(ref p) if p == "fresh.txt"));
        assert!(!tmp.dir.join("fresh.txt").exists(), "the new file is removed");
    }

    #[test]
    fn b1_revert_batch_reverts_both_files() {
        let (tmp, backend, _c1, c2) = build_repo();
        // A 2-file batch: the loader calls revert_file per path; both must apply.
        let m = backend.revert_file(&c2, "a.go").unwrap();
        let a = backend.revert_file(&c2, "b.go").unwrap();
        assert!(matches!(m, RevertOutcome::Overwritten(_)));
        assert!(matches!(a, RevertOutcome::Deleted(_)));
        assert_eq!(
            fs::read_to_string(tmp.dir.join("a.go")).unwrap(),
            "package main\n\nfunc A() {}\n",
            "a.go reverted to parent bytes"
        );
        assert!(!tmp.dir.join("b.go").exists(), "b.go removed in the same batch");
    }

    #[test]
    fn b1_revert_bare_repo_errors() {
        let tmp = TempRepo::new();
        let repo = Repository::init_bare(&tmp.dir).unwrap();
        // A bare repo has no work tree -> revert must error, not write anywhere.
        let err = revert_file_in(&repo, "HEAD", "a.go").unwrap_err();
        assert!(err.0.contains("bare") || err.0.contains("working tree"), "bare -> Err: {}", err.0);
    }

    #[test]
    fn b1_revert_path_traversal_rejected_no_outside_write() {
        let (tmp, backend, _c1, c2) = build_repo();
        // A workdir-escaping path must be rejected BEFORE any write. Drop a sentinel
        // one level above the work tree; the traversal target must never appear.
        let sentinel = tmp.dir.parent().unwrap().join(format!(
            "gitgit-b1-sentinel-{}-{}",
            std::process::id(),
            "traversal"
        ));
        let _ = fs::remove_file(&sentinel);
        let escaping = format!("../{}", sentinel.file_name().unwrap().to_str().unwrap());
        let err = backend.revert_file(&c2, &escaping).unwrap_err();
        assert!(err.0.contains("unsafe revert path"), "traversal -> Err: {}", err.0);
        assert!(!sentinel.exists(), "rejected path wrote nothing outside the work tree");
        // Absolute paths are likewise rejected.
        assert!(backend.revert_file(&c2, "/etc/passwd").is_err(), "absolute path rejected");
    }

    /// Write raw `bytes` to `name` under the repo dir (for binary-file fixtures).
    fn write_bytes(root: &Path, name: &str, bytes: &[u8]) {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn b1_binary_change_previews_as_a_binary_notice_not_a_blank_diff() {
        // A binary file modified across two commits: libgit2 emits a binary delta with
        // ZERO patch body lines, so without detection file_view would return an empty
        // FileView::Diff that renders as a blank body. It must instead be a Binary view
        // carrying a notice so the user can tell the file is binary AND changed.
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        write_bytes(&tmp.dir, "data.bin", &[0x00, 0x01, 0x02, 0x03, 0x04, 0x05]);
        let c1 = commit_all(&repo, "add binary", None);
        write_bytes(&tmp.dir, "data.bin", &[0x00, 0xff, 0xfe, 0xfd, 0x00, 0x80, 0x01]);
        let c2 = commit_all(&repo, "change binary", Some(c1));
        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();

        let view = backend.file_view(&short_hash(c2), "data.bin").unwrap().unwrap();
        match view {
            FileView::Binary(b) => {
                assert_eq!(b.path, "data.bin");
                assert!(!b.note.is_empty(), "the binary notice is non-empty");
            }
            other => panic!("a changed binary must preview as Binary, got {other:?}"),
        }
    }

    #[test]
    fn b1_binary_added_at_root_previews_as_a_binary_notice() {
        // An ADDED binary at the root (NUL bytes) also yields a 0-hunk binary delta.
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        write_bytes(&tmp.dir, "image.bin", &[0, 159, 146, 150, 0, 255, 1, 2, 3]);
        let c1 = commit_all(&repo, "add binary blob", None);
        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();
        let view = backend.file_view(&short_hash(c1), "image.bin").unwrap().unwrap();
        assert!(
            matches!(view, FileView::Binary(_)),
            "an added binary blob previews as a Binary notice, not an empty diff"
        );
    }

    #[test]
    fn b1_commit_cap_zero_is_clamped_to_one_not_an_empty_log() {
        // A typo'd commit_cap = 0 must NOT silently truncate the whole log over a
        // non-empty repo; it clamps to >= 1 so at least HEAD loads.
        let (tmp, _backend, _c1, _c2) = build_repo();
        let mut config = Config::default();
        config.behavior.commit_cap = 0;
        let backend = RealBackend::open(&tmp.dir, &config).unwrap();
        let snap = backend.load_repo().unwrap();
        assert!(!snap.commits.is_empty(), "commit_cap=0 clamps to >=1, the log is not blank");
    }

    /// Build a repo whose c2 changes TWO well-separated lines of m.go (so the diff
    /// has two distinct hunks). Returns (tmp, backend, short c2).
    fn build_two_hunk_repo() -> (TempRepo, RealBackend, String) {
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        let v1 = "L1\nL2old\nL3\nL4\nL5\nL6\nL7\nL8\nL9\nL10\nL11\nL12\nL13\nL14old\nL15\nL16\n";
        write(&tmp.dir, "m.go", v1);
        let c1 = commit_all(&repo, "root", None);
        // Change line 2 and line 14 - far enough apart (context gap) -> two hunks.
        let v2 = "L1\nL2new\nL3\nL4\nL5\nL6\nL7\nL8\nL9\nL10\nL11\nL12\nL13\nL14new\nL15\nL16\n";
        write(&tmp.dir, "m.go", v2);
        let c2 = commit_all(&repo, "two changes", Some(c1));
        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();
        (tmp, backend, short_hash(c2))
    }

    #[test]
    fn b1_full_context_keeps_the_middle_and_distinct_hunks_without_a_gap_marker() {
        // m.go (16 lines) changes L2 and L14. The backend now emits FULL context (folding the
        // unchanged middle is the View layer's "Hide unchanged" job), so there is NO gap
        // marker and every unchanged line (incl. L6..L10) is present - BUT the two far-apart
        // changes must still carry DISTINCT hunk indices (restamped from a default-context
        // pass) so per-hunk revert stays aligned with the displayed grouping.
        let (_tmp, backend, c2) = build_two_hunk_repo();
        let FileView::Diff(d) = backend.file_view(&c2, "m.go").unwrap().unwrap() else {
            panic!("m.go has a diff");
        };
        assert!(d.lines.iter().all(|l| l.fold.is_none()), "no backend gap markers (full context)");
        assert!(d.lines.iter().any(|l| l.new_no == Some(8)), "the omitted middle (L8) is present");
        let max_hunk = d.lines.iter().map(|l| l.hunk).max().unwrap();
        assert_eq!(max_hunk, 1, "the two far-apart changes keep distinct hunks");
    }

    #[test]
    fn b1_first_change_below_line_one_has_no_leading_marker_in_full_context() {
        // m.go (8 lines) changes only L6. Full context emits the whole file with NO leading
        // gap marker; L1 is the first row (a real Context line), not a synthetic marker.
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        write(&tmp.dir, "m.go", "L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\n");
        let c1 = commit_all(&repo, "root", None);
        write(&tmp.dir, "m.go", "L1\nL2\nL3\nL4\nL5\nL6new\nL7\nL8\n");
        let c2 = commit_all(&repo, "late change", Some(c1));
        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();
        let FileView::Diff(d) = backend.file_view(&short_hash(c2), "m.go").unwrap().unwrap() else {
            panic!("m.go has a diff");
        };
        let first = d.lines.first().unwrap();
        assert!(first.fold.is_none(), "no leading gap marker (full context)");
        assert_eq!(first.new_no, Some(1), "the file starts at L1");
        assert!(d.lines.iter().all(|l| l.fold.is_none()), "no markers anywhere");
    }

    #[test]
    fn b1_oversized_file_falls_back_to_compact_context_and_still_shows_the_change() {
        // The 16-line file overflows a tiny cap of 7 under FULL context, so `diff_view` falls
        // back to DEFAULT (compact) context - which keeps the diff bounded AND never truncates
        // the change away. The compact path re-introduces gap markers; the marker must NEVER be
        // the final row (the cursor/normalize logic assumes every marker is followed by a real
        // line), and the L2 change must be visible (not lost to an all-context truncation).
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        let v1 = "L1\nL2old\nL3\nL4\nL5\nL6\nL7\nL8\nL9\nL10\nL11\nL12\nL13\nL14old\nL15\nL16\n";
        write(&tmp.dir, "m.go", v1);
        let c1 = commit_all(&repo, "root", None);
        let v2 = "L1\nL2new\nL3\nL4\nL5\nL6\nL7\nL8\nL9\nL10\nL11\nL12\nL13\nL14new\nL15\nL16\n";
        write(&tmp.dir, "m.go", v2);
        let c2 = commit_all(&repo, "two changes", Some(c1));
        let mut config = Config::default();
        config.behavior.preview_line_cap = 7;
        let backend = RealBackend::open(&tmp.dir, &config).unwrap();
        let FileView::Diff(d) = backend.file_view(&short_hash(c2), "m.go").unwrap().unwrap() else {
            panic!("m.go has a diff");
        };
        assert_eq!(d.lines.len(), 7, "emission stops at the cap");
        assert!(d.lines.last().unwrap().fold.is_none(), "the diff never ends on a synthetic marker");
        assert!(
            d.lines.iter().any(|l| l.kind == LineKind::Added && l.new_no == Some(2)),
            "the L2 change is shown (compact fallback did not truncate it away)"
        );
    }

    #[test]
    fn b1_revert_hunk_reverts_only_the_targeted_hunk() {
        let (tmp, backend, c2) = build_two_hunk_repo();
        // Sanity: the diff has two hunks (lines carry hunk 0 and hunk 1).
        let FileView::Diff(d) = backend.file_view(&c2, "m.go").unwrap().unwrap() else {
            panic!("m.go has a diff");
        };
        let max_hunk = d.lines.iter().map(|l| l.hunk).max().unwrap();
        assert_eq!(max_hunk, 1, "the change splits into two hunks");

        // Revert ONLY hunk 0 (the L2 change): L2 returns to old, L14 stays new.
        backend.revert_hunk(&c2, "m.go", 0).unwrap();
        let after = fs::read_to_string(tmp.dir.join("m.go")).unwrap();
        assert!(after.contains("L2old"), "hunk 0 reverted: L2 back to old");
        assert!(after.contains("L14new"), "hunk 1 untouched: L14 still new");
        assert!(!after.contains("L2new"));
    }

    #[test]
    fn b1_revert_hunk_out_of_range_errors() {
        let (_tmp, backend, c2) = build_two_hunk_repo();
        assert!(backend.revert_hunk(&c2, "m.go", 99).is_err(), "no such hunk index");
    }

    #[test]
    fn b1_read_worktree_returns_on_disk_text() {
        let (_tmp, backend, _c1, _c2) = build_repo();
        // The working tree holds c2's content (a.go was modified in c2).
        let text = backend.read_worktree("a.go").unwrap();
        assert_eq!(text, "package main\n\nfunc A() { return }\n");
    }

    #[test]
    fn b1_write_then_read_worktree_round_trips() {
        let (tmp, backend, _c1, _c2) = build_repo();
        let new = "package main\n\nfunc A() { panic(\"x\") }\n";
        backend.write_worktree("a.go", new).unwrap();
        // The on-disk file now holds the edited content...
        assert_eq!(fs::read_to_string(tmp.dir.join("a.go")).unwrap(), new);
        // ...and a fresh read sees it.
        assert_eq!(backend.read_worktree("a.go").unwrap(), new);
    }

    #[test]
    fn b1_write_worktree_creates_new_file_and_dirs() {
        let (tmp, backend, _c1, _c2) = build_repo();
        backend.write_worktree("pkg/new.go", "package pkg\n").unwrap();
        assert_eq!(fs::read_to_string(tmp.dir.join("pkg/new.go")).unwrap(), "package pkg\n");
    }

    #[test]
    fn b1_read_worktree_rejects_escaping_path() {
        let (_tmp, backend, _c1, _c2) = build_repo();
        // A `..` traversal must be rejected by the same safe_join gate as revert.
        assert!(backend.read_worktree("../escape.txt").is_err());
        assert!(backend.write_worktree("../escape.txt", "x").is_err());
    }

    #[test]
    fn b1_read_worktree_missing_file_errors() {
        let (_tmp, backend, _c1, _c2) = build_repo();
        assert!(backend.read_worktree("does/not/exist.go").is_err());
    }

    /// Flatten a TreeNode tree into (file name, status) pairs for assertions.
    fn collect_files(nodes: &[TreeNode], out: &mut Vec<(String, FileStatus)>) {
        for node in nodes {
            match node {
                TreeNode::Dir { children, .. } => collect_files(children, out),
                TreeNode::File { name, status } => out.push((name.clone(), *status)),
            }
        }
    }

    // -- new write ops: branch / tag at a commit, reword (HEAD + rebase) --------

    /// HEAD subject ("%s") and full message ("%B") via the system git, for assertions.
    fn head_subject(dir: &Path) -> String {
        run_git(dir, &["log", "-1", "--format=%s"]).unwrap()
    }
    fn commit_message(dir: &Path, rev: &str) -> String {
        run_git(dir, &["log", "-1", "--format=%B", rev]).unwrap()
    }

    /// Full oid of `rev` via the system git (length-stable, unlike `--short`).
    fn full_oid(dir: &Path, rev: &str) -> String {
        run_git(dir, &["rev-parse", rev]).unwrap()
    }

    #[test]
    fn branch_create_at_commit_no_checkout_leaves_head() {
        let (tmp, backend, c1, _c2) = build_repo();
        backend.branch_create("topic", &c1, false).unwrap();
        // The branch points at c1, and HEAD did NOT move to it.
        assert_eq!(full_oid(&tmp.dir, "topic"), full_oid(&tmp.dir, &c1), "branch at target");
        let cur = run_git(&tmp.dir, &["branch", "--show-current"]).unwrap();
        assert_ne!(cur, "topic", "no-checkout leaves HEAD on the original branch");
    }

    #[test]
    fn branch_create_with_checkout_switches() {
        let (tmp, backend, _c1, c2) = build_repo();
        // Checkout at HEAD (c2) is a clean no-tree-change switch.
        backend.branch_create("feature", &c2, true).unwrap();
        assert_eq!(run_git(&tmp.dir, &["branch", "--show-current"]).unwrap(), "feature");
    }

    #[test]
    fn tag_at_commit_points_at_target() {
        let (tmp, backend, c1, _c2) = build_repo();
        backend.tag_at("v0", &c1).unwrap();
        assert_eq!(full_oid(&tmp.dir, "v0^{commit}"), full_oid(&tmp.dir, &c1), "tag at target");
    }

    #[test]
    fn checkout_detaches_head_at_target() {
        let (tmp, backend, c1, _c2) = build_repo();
        let summary = backend.checkout(&c1).unwrap();
        assert!(summary.contains("detached"), "summary names the detach: {summary:?}");
        // HEAD now resolves to c1 directly and is not on any branch (detached).
        assert_eq!(full_oid(&tmp.dir, "HEAD"), full_oid(&tmp.dir, &c1), "HEAD at the target");
        let cur = run_git(&tmp.dir, &["branch", "--show-current"]).unwrap();
        assert!(cur.is_empty(), "checkout of a raw hash detaches HEAD, got branch {cur:?}");
    }

    #[test]
    fn load_repo_walks_all_branches_not_just_head() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // A divergent commit on a side branch: NOT an ancestor of HEAD, so a HEAD-only
        // walk would miss it. The all-refs walk must surface it (cherry-pick needs it).
        run_git(&tmp.dir, &["checkout", "-b", "side"]).unwrap();
        write(&tmp.dir, "side.txt", "side only\n");
        run_git(&tmp.dir, &["add", "side.txt"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "side: divergent commit"]).unwrap();
        let side = full_oid(&tmp.dir, "side");
        run_git(&tmp.dir, &["checkout", "-"]).unwrap(); // back to the base branch
        let snap = backend.load_repo().unwrap();
        assert!(
            snap.commits.iter().any(|c| c.full_hash == side),
            "the log must include the divergent side-branch commit, not just HEAD ancestry"
        );
    }

    #[test]
    fn working_row_parent_is_head_not_the_newest_branch_tip() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // A sibling branch gets a commit AFTER HEAD's tip, so the all-refs walk yields
        // it FIRST (newest by topo+time) - but HEAD stays on the base branch. The
        // <current> row's HEAD parent must be the real HEAD, NOT that newest sibling.
        run_git(&tmp.dir, &["checkout", "-b", "newer"]).unwrap();
        write(&tmp.dir, "newer.txt", "newer\n");
        run_git(&tmp.dir, &["add", "newer.txt"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "newer: sibling tip"]).unwrap();
        run_git(&tmp.dir, &["checkout", "-"]).unwrap(); // HEAD back on the base branch
        let head = Oid::from_str(&full_oid(&tmp.dir, "HEAD")).unwrap();
        let newer = Oid::from_str(&full_oid(&tmp.dir, "newer")).unwrap();
        let snap = backend.load_repo().unwrap();
        let working = &snap.commits[0];
        assert!(working.is_working, "row 0 is the synthetic <current> row");
        assert_eq!(
            working.parents,
            vec![short_hash(head)],
            "the working row's HEAD parent must be the real HEAD, not the newest sibling tip"
        );
        assert_ne!(working.parents, vec![short_hash(newer)], "not the newer sibling");
    }

    #[test]
    fn cherry_pick_adds_the_commit_onto_head() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // A side-branch commit touching a NEW file picks cleanly onto the base branch.
        run_git(&tmp.dir, &["checkout", "-b", "side"]).unwrap();
        write(&tmp.dir, "picked.txt", "from the picked commit\n");
        run_git(&tmp.dir, &["add", "picked.txt"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "side: add picked.txt"]).unwrap();
        let pick = full_oid(&tmp.dir, "HEAD");
        run_git(&tmp.dir, &["checkout", "-"]).unwrap(); // back to the base branch (at c2)
        let before = full_oid(&tmp.dir, "HEAD");
        let summary = backend.cherry_pick(&pick).unwrap();
        assert!(summary.contains("Cherry-picked"), "summary: {summary:?}");
        assert_ne!(full_oid(&tmp.dir, "HEAD"), before, "a new commit landed on the branch");
        assert!(tmp.dir.join("picked.txt").exists(), "the picked file is in the tree");
    }

    #[test]
    fn cherry_pick_conflict_aborts_leaving_no_partial_state() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // Side and base each change a.go's func line a DIFFERENT way (both off c2's
        // "return"), so picking the side commit onto the diverged base conflicts.
        run_git(&tmp.dir, &["checkout", "-b", "side"]).unwrap();
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { panic(\"x\") }\n");
        run_git(&tmp.dir, &["commit", "-am", "side: a.go panic"]).unwrap();
        let pick = full_oid(&tmp.dir, "HEAD");
        run_git(&tmp.dir, &["checkout", "-"]).unwrap();
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { log() }\n");
        run_git(&tmp.dir, &["commit", "-am", "base: a.go log"]).unwrap();
        let before = full_oid(&tmp.dir, "HEAD");
        let err = backend.cherry_pick(&pick).unwrap_err();
        assert!(!err.to_string().is_empty(), "the conflict error is surfaced");
        assert_eq!(full_oid(&tmp.dir, "HEAD"), before, "HEAD rolled back after the abort");
        assert!(
            !tmp.dir.join(".git/CHERRY_PICK_HEAD").exists(),
            "no half-applied cherry-pick remains"
        );
    }

    #[test]
    fn revert_commit_conflict_aborts_leaving_no_partial_state() {
        let (tmp, backend, _c1, c2) = build_repo();
        // A later commit rewrites the SAME line c2 touched, so reverting c2 (undoing its
        // edit) conflicts against the current content -> the abort must roll back cleanly.
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { log() }\n");
        run_git(&tmp.dir, &["commit", "-am", "later: a.go log"]).unwrap();
        let before = full_oid(&tmp.dir, "HEAD");
        let err = backend.revert_commit(&c2).unwrap_err();
        assert!(!err.to_string().is_empty(), "the conflict error is surfaced");
        assert_eq!(full_oid(&tmp.dir, "HEAD"), before, "HEAD rolled back after the abort");
        assert!(
            !tmp.dir.join(".git/REVERT_HEAD").exists(),
            "no half-applied revert remains"
        );
    }

    #[test]
    fn revert_commit_adds_an_inverse_commit() {
        let (tmp, backend, _c1, c2) = build_repo();
        // c2 added b.go; reverting c2 must remove it again in a new inverse commit.
        let before = full_oid(&tmp.dir, "HEAD");
        let summary = backend.revert_commit(&c2).unwrap();
        assert!(summary.contains("Reverted"), "summary: {summary:?}");
        assert_ne!(full_oid(&tmp.dir, "HEAD"), before, "a new inverse commit was added");
        assert!(!tmp.dir.join("b.go").exists(), "revert removed the file c2 had added");
    }

    #[test]
    fn reset_hard_moves_branch_and_discards_changes() {
        let (tmp, backend, c1, _c2) = build_repo();
        // Dirty the tree, then hard-reset to c1: the branch moves AND changes vanish.
        write(&tmp.dir, "a.go", "package main\n\n// scratch\n");
        backend.reset(&c1, "--hard").unwrap();
        assert_eq!(full_oid(&tmp.dir, "HEAD"), full_oid(&tmp.dir, &c1), "branch moved to c1");
        let status = run_git(&tmp.dir, &["status", "--porcelain"]).unwrap();
        assert!(status.is_empty(), "hard reset left a clean tree, got: {status:?}");
        assert!(!tmp.dir.join("b.go").exists(), "c2's added file removed by the hard reset");
    }

    #[test]
    fn reset_soft_moves_branch_but_keeps_changes_staged() {
        let (tmp, backend, c1, _c2) = build_repo();
        backend.reset(&c1, "--soft").unwrap();
        assert_eq!(full_oid(&tmp.dir, "HEAD"), full_oid(&tmp.dir, &c1), "branch moved to c1");
        // soft keeps the index, so c2's add is now STAGED.
        let staged = run_git(&tmp.dir, &["diff", "--cached", "--name-only"]).unwrap();
        assert!(staged.contains("b.go"), "soft reset keeps c2's add staged: {staged:?}");
    }

    #[test]
    fn undo_commit_soft_resets_the_tip() {
        let (tmp, backend, c1, _c2) = build_repo();
        backend.undo_commit().unwrap();
        assert_eq!(full_oid(&tmp.dir, "HEAD"), full_oid(&tmp.dir, &c1), "HEAD moved back to c1");
        // The undone commit's changes are kept staged (soft reset).
        let staged = run_git(&tmp.dir, &["diff", "--cached", "--name-only"]).unwrap();
        assert!(staged.contains("b.go"), "the undone commit's changes are staged: {staged:?}");
    }

    #[test]
    fn create_patch_writes_a_patch_that_git_apply_accepts() {
        let (tmp, backend, c1, c2) = build_repo();
        let dest = tmp.dir.join("c2.patch");
        let path = dest.to_str().unwrap();
        let summary = backend.create_patch(&c2, path).unwrap();
        assert_eq!(summary, format!("Wrote {path}"));

        let patch = fs::read_to_string(&dest).unwrap();
        // The mbox header + this commit's subject + a unified-diff body for its files.
        assert!(patch.starts_with("From "), "mbox patch starts with a From line: {patch:.40?}");
        assert!(patch.contains("second: change, add, delete"), "carries the commit subject");
        assert!(patch.contains("diff --git a/b.go b/b.go"), "carries the added file's diff");
        // Byte-exact capture: the trailing newline must survive (the no-trim guard).
        assert!(patch.ends_with('\n'), "the patch keeps its trailing newline (not trimmed)");

        // PROVE it is actually applyable (not just that it looks like a patch): rewind the
        // working tree to the PARENT and `git apply --check` the patch - it must apply
        // cleanly. This is the property `run_git_bytes` (no trim) exists to guarantee.
        run_git(&tmp.dir, &["checkout", &c1]).unwrap();
        run_git(&tmp.dir, &["apply", "--check", path]).expect("the written patch applies on its parent");
    }

    #[test]
    fn working_patch_captures_local_changes_and_applies() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // Clean tree (build_repo committed everything): no local changes -> empty patch,
        // and create_working_patch refuses to write a zero-byte file.
        assert!(backend.working_patch("a.go").unwrap().is_empty(), "a clean file has no local diff");
        let dest = tmp.dir.join("a.patch");
        let path = dest.to_str().unwrap();
        assert!(
            backend.create_working_patch("a.go", path).is_err(),
            "no local changes -> no patch written"
        );
        assert!(!dest.exists(), "the empty case writes no file");

        // Modify the working tree, then the patch carries the change byte-exact and applies.
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { return /*x*/ }\n");
        let patch = backend.working_patch("a.go").unwrap();
        assert!(patch.contains("diff --git a/a.go b/a.go"), "carries the file's working diff");
        assert!(patch.contains("/*x*/"), "carries the edited content");
        assert!(patch.ends_with('\n'), "keeps the trailing newline (no-trim, so git apply accepts it)");

        let summary = backend.create_working_patch("a.go", path).unwrap();
        assert_eq!(summary, format!("Wrote {path}"));
        // PROVE applyable: restore HEAD's a.go, then `git apply --check` the written patch.
        run_git(&tmp.dir, &["checkout", "--", "a.go"]).unwrap();
        run_git(&tmp.dir, &["apply", "--check", path]).expect("the working patch applies on HEAD");
    }

    #[test]
    fn whole_tree_patch_captures_every_file_and_apply_patch_round_trips() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // Dirty two tracked files + add an untracked one. An EMPTY `file` = the WHOLE tree.
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { return /*x*/ }\n");
        write(&tmp.dir, "keep.txt", "edited\n");
        write(&tmp.dir, "fresh.txt", "brand new\n");
        let patch = backend.working_patch("").unwrap();
        assert!(patch.contains("a/a.go"), "covers a tracked edit");
        assert!(patch.contains("keep.txt"), "covers a second tracked edit");
        assert!(patch.contains("fresh.txt") && patch.contains("new file mode"), "covers the untracked add");

        let dest = tmp.dir.join("all.patch");
        let path = dest.to_str().unwrap();
        backend.create_working_patch("", path).unwrap();

        // Reset every change, then apply_patch restores all of them (tracked + new file).
        run_git(&tmp.dir, &["reset", "--hard", "HEAD"]).unwrap();
        std::fs::remove_file(tmp.dir.join("fresh.txt")).ok();
        backend.apply_patch(path).unwrap();
        assert!(fs::read_to_string(tmp.dir.join("a.go")).unwrap().contains("/*x*/"), "tracked edit re-applied");
        assert_eq!(fs::read_to_string(tmp.dir.join("keep.txt")).unwrap(), "edited\n", "second edit re-applied");
        assert_eq!(fs::read_to_string(tmp.dir.join("fresh.txt")).unwrap(), "brand new\n", "new file re-applied");

        // A bad patch path errors (Notice), leaving the tree intact.
        assert!(backend.apply_patch("/tmp/does-not-exist-xyz.patch").is_err(), "missing patch errors");
    }

    #[test]
    fn working_patch_handles_an_unborn_head_repo() {
        // A fresh repo with ZERO commits (unborn HEAD): `git diff HEAD` errors, so the patch
        // must fall through to the /dev/null add-path instead of surfacing "bad revision HEAD".
        let tmp = TempRepo::new();
        Repository::init(&tmp.dir).unwrap();
        write(&tmp.dir, "first.txt", "hello\nworld\n");
        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();
        let patch = backend.working_patch("first.txt").unwrap();
        assert!(patch.contains("diff --git a/first.txt b/first.txt"), "no-HEAD untracked file still diffs");
        assert!(patch.contains("new file mode"), "it is an add patch");
        assert!(patch.contains("+hello"), "carries the content");

        // A STAGED-but-uncommitted file in the same unborn-HEAD repo: `ls-files --others` skips
        // it (it is in the index), so the patch must come from `git diff --cached`.
        write(&tmp.dir, "staged.txt", "indexed\n");
        run_git(&tmp.dir, &["add", "staged.txt"]).unwrap();
        let staged = backend.working_patch("staged.txt").unwrap();
        assert!(staged.contains("b/staged.txt") && staged.contains("+indexed"), "staged unborn-HEAD file is captured: {staged}");
    }

    #[test]
    fn working_patch_handles_an_untracked_new_file() {
        // `git diff HEAD` omits untracked files, but the files menu offers the patch items
        // for them (folded in as Added), so working_patch must fall back to a /dev/null diff
        // - else Copy/Create Patch would silently fail on the common "new file" case.
        let (tmp, backend, _c1, _c2) = build_repo();
        write(&tmp.dir, "fresh.txt", "brand new\nlines\n");
        let patch = backend.working_patch("fresh.txt").unwrap();
        assert!(patch.contains("diff --git a/fresh.txt b/fresh.txt"), "an untracked file gets a diff");
        assert!(patch.contains("new file mode"), "it is an add (new-file) patch");
        assert!(patch.contains("+brand new"), "carries the new content");

        // A tracked-but-unchanged file stays EMPTY (no spurious full-add via the fallback).
        assert!(backend.working_patch("keep.txt").unwrap().is_empty(), "unchanged tracked file = no patch");

        // The written patch applies on the current tree (the file does not yet exist in it).
        let dest = tmp.dir.join("fresh.patch");
        let path = dest.to_str().unwrap();
        backend.create_working_patch("fresh.txt", path).unwrap();
        fs::remove_file(tmp.dir.join("fresh.txt")).unwrap();
        run_git(&tmp.dir, &["apply", "--check", path]).expect("the untracked new-file patch applies");
    }

    #[test]
    fn working_patch_over_a_folder_includes_tracked_and_untracked_files() {
        // A folder Copy/Create Patch passes a DIRECTORY prefix. `git diff HEAD -- <dir>` omits
        // untracked files, so the patch must ALSO sweep new files under the prefix - else a
        // folder with a modified file silently drops its sibling new file, and a new-only
        // folder errors. Both cases must yield a complete, applyable patch.
        let (tmp, backend, _c1, _c2) = build_repo();
        write(&tmp.dir, "pkg/mod.go", "package pkg\n");
        run_git(&tmp.dir, &["add", "pkg/mod.go"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "seed pkg/mod.go"]).unwrap();
        // Modify the tracked file AND add a brand-new untracked file under the same folder.
        write(&tmp.dir, "pkg/mod.go", "package pkg\n\nfunc M() {}\n");
        write(&tmp.dir, "pkg/new.go", "package pkg\n\nfunc New() {}\n");

        let patch = backend.working_patch("pkg").unwrap();
        assert!(patch.contains("b/pkg/mod.go"), "carries the tracked-modified file");
        assert!(patch.contains("func M()"), "carries the modified content");
        assert!(patch.contains("b/pkg/new.go") && patch.contains("new file mode"), "INCLUDES the untracked new file");
        assert!(patch.contains("func New()"), "carries the new file's content");

        // The folder patch applies on the current tree once the new file is removed + the
        // tracked file restored to HEAD.
        let dest = tmp.dir.join("pkg.patch");
        let path = dest.to_str().unwrap();
        backend.create_working_patch("pkg", path).unwrap();
        run_git(&tmp.dir, &["checkout", "--", "pkg/mod.go"]).unwrap();
        fs::remove_file(tmp.dir.join("pkg/new.go")).unwrap();
        run_git(&tmp.dir, &["apply", "--check", path]).expect("the folder patch applies on HEAD");

        // A NEW-ONLY folder (no tracked changes) still yields a non-empty add patch, not an error.
        write(&tmp.dir, "fresh/a.txt", "aaa\n");
        write(&tmp.dir, "fresh/b.txt", "bbb\n");
        let only_new = backend.working_patch("fresh").unwrap();
        assert!(only_new.contains("b/fresh/a.txt") && only_new.contains("b/fresh/b.txt"), "both new files: {only_new}");
    }

    #[test]
    fn working_patch_handles_a_non_ascii_untracked_filename() {
        // `git ls-files` C-quotes non-ASCII names under `core.quotePath` (default on); the `-z`
        // sweep must reach the REAL path so the new file is captured instead of aborting the
        // whole patch on a quoted literal that `git diff --no-index` cannot resolve.
        let (tmp, backend, _c1, _c2) = build_repo();
        write(&tmp.dir, "uni/na\u{ef}ve.js", "const x = 1;\n"); // naïve.js
        let patch = backend.working_patch("uni").unwrap();
        assert!(patch.contains("na\u{ef}ve.js"), "the non-ASCII new file is in the patch: {patch}");
        assert!(patch.contains("new file mode") && patch.contains("const x"), "it is a complete add patch");
    }

    #[test]
    fn revision_source_reads_a_files_head_content_and_misses_absent_paths() {
        use crate::diff::FileView;
        let (_tmp, backend, _c1, _c2) = build_repo();

        // HEAD's a.go is the second commit's content (raw, un-highlighted Source lines).
        match backend.revision_source("HEAD", "a.go").unwrap() {
            Some(FileView::Source(s)) => {
                let text: String = s
                    .lines
                    .iter()
                    .map(|toks| toks.iter().map(|t| t.text.as_str()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("func A() { return }"), "HEAD content of a.go: {text:?}");
                assert_eq!(s.path, "a.go");
            }
            other => panic!("a tracked file's HEAD revision is a Source: {other:?}"),
        }

        // A path deleted by HEAD (src/gone.go) does not exist there -> None (no overlay).
        assert!(backend.revision_source("HEAD", "src/gone.go").unwrap().is_none(), "gone at HEAD");
        // A never-existing path -> None too.
        assert!(backend.revision_source("HEAD", "nope.txt").unwrap().is_none(), "absent path");
    }

    #[test]
    fn blame_annotates_each_line_with_the_commit_that_last_changed_it() {
        use crate::diff::FileView;
        let (_tmp, backend, c1, c2) = build_repo();
        // a.go at HEAD: "package main\n\nfunc A() { return }\n". Line 1 is from c1 (added there,
        // unchanged in c2); line 3 was rewritten by c2. Blame the clean working tree (WORKING_REV).
        let Some(FileView::Blame(b)) = backend.blame(WORKING_REV, "a.go").unwrap() else {
            panic!("a tracked file blames to a Blame view");
        };
        assert_eq!(b.path, "a.go");
        assert_eq!(b.lines.len(), 3, "three source lines");
        let text: String = b.lines.iter().map(|l| line_text(&l.tokens)).collect::<Vec<_>>().join("|");
        assert_eq!(text, "package main||func A() { return }", "raw source per line: {text}");
        assert_eq!(b.lines[0].commit, c1, "line 1 last changed by the root commit");
        assert_eq!(b.lines[2].commit, c2, "line 3 last changed by the second commit");
        assert!(b.lines.iter().all(|l| l.author == "Test Author"), "author from the signature");
        assert!(b.lines.iter().all(|l| l.date == "22.05.2026"), "date in the configured Dmy format");
    }

    #[test]
    fn blame_marks_an_uncommitted_working_line_not_committed_yet() {
        use crate::diff::FileView;
        let (tmp, backend, _c1, _c2) = build_repo();
        // Append an unsaved line to the working tree; blaming WORKING_REV must flag it.
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { return }\nvar fresh = 1\n");
        let Some(FileView::Blame(b)) = backend.blame(WORKING_REV, "a.go").unwrap() else {
            panic!("the dirty working file still blames");
        };
        assert_eq!(b.lines.len(), 4, "the appended line is annotated too");
        let last = b.lines.last().unwrap();
        assert!(last.commit.is_empty(), "an uncommitted line has a blank short hash: {:?}", last.commit);
        assert_eq!(last.author, "Not Committed Yet", "git marks the unsaved line");
        assert!(!b.lines[0].commit.is_empty(), "a committed line keeps its hash");
    }

    #[test]
    fn blame_of_an_absent_path_is_none() {
        let (_tmp, backend, _c1, _c2) = build_repo();
        assert!(backend.blame(WORKING_REV, "nope.txt").unwrap().is_none(), "untracked path -> no blame");
        assert!(backend.blame("HEAD", "src/gone.go").unwrap().is_none(), "deleted-at-HEAD path -> no blame");
    }

    #[test]
    fn parse_blame_porcelain_caches_metadata_and_flags_uncommitted() {
        // A hand-built porcelain stream: a real commit on lines 1-2 (its metadata emitted only on
        // the first appearance, cached for line 2), then an all-zero (uncommitted) line 3.
        let sha = "abcdef1234567890abcdef1234567890abcdef12";
        let porcelain = format!(
            "{sha} 1 1 2\n\
             author Alice\nauthor-mail <a@x>\nauthor-time 1700000000\nauthor-tz +0200\n\
             committer Alice\ncommitter-time 1700000000\ncommitter-tz +0200\nsummary first\nfilename a.txt\n\
             \tline one\n\
             {sha} 2 2\n\
             \tline two\n\
             0000000000000000000000000000000000000000 3 3 1\n\
             author Not Committed Yet\nauthor-time 1700000100\nauthor-tz +0000\nsummary x\nfilename a.txt\n\
             \tline three\n"
        );
        let lines = parse_blame_porcelain(porcelain.as_bytes(), DateFormat::Dmy);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].commit, "abcdef12", "8-char short hash");
        assert_eq!(lines[0].author, "Alice");
        assert_eq!(line_text(&lines[0].tokens), "line one");
        // Line 2 repeats the sha with NO metadata block -> author/date served from the cache.
        assert_eq!(lines[1].commit, "abcdef12");
        assert_eq!(lines[1].author, "Alice", "repeated commit reuses cached author");
        assert_eq!(lines[1].date, lines[0].date, "same cached date");
        // Line 3 is the all-zero uncommitted sha.
        assert!(lines[2].commit.is_empty(), "uncommitted -> blank hash");
        assert_eq!(lines[2].author, "Not Committed Yet");
    }

    #[test]
    fn parse_blame_tz_decodes_signed_offsets() {
        assert_eq!(parse_blame_tz("+0000"), 0);
        assert_eq!(parse_blame_tz("+0200"), 120);
        assert_eq!(parse_blame_tz("-0530"), -330);
        assert_eq!(parse_blame_tz("garbage"), 0, "malformed -> UTC");
    }

    #[test]
    fn file_revisions_lists_only_commits_that_touched_the_path() {
        let (_tmp, backend, c1, c2) = build_repo();
        // a.go changed in BOTH commits (added in c1, modified in c2).
        let a = backend.file_revisions("a.go").unwrap();
        let hashes: Vec<&str> = a.iter().map(|(h, _)| h.as_str()).collect();
        assert!(hashes.contains(&c2.as_str()) && hashes.contains(&c1.as_str()), "a.go: both commits {hashes:?}");
        assert!(a[0].1.contains(&c2), "label carries the short hash");
        // Label is `hash  date  subject`: the date (default Dmy `DD.MM.YYYY`) prefixes the
        // subject so two same-subject commits stay distinguishable.
        let parts: Vec<&str> = a[0].1.splitn(3, "  ").collect();
        assert_eq!(parts.len(), 3, "label = hash + date + subject: {:?}", a[0].1);
        assert!(parts[1].matches('.').count() == 2, "the date field is DD.MM.YYYY: {:?}", parts[1]);

        // b.go was ADDED in c2 only -> just c2 in its history.
        let b = backend.file_revisions("b.go").unwrap();
        assert_eq!(b.len(), 1, "b.go has one revision");
        assert_eq!(b[0].0, c2, "the commit that added it");

        // keep.txt was added in c1 and untouched since -> only c1.
        let k = backend.file_revisions("keep.txt").unwrap();
        assert_eq!(k.iter().map(|(h, _)| h.clone()).collect::<Vec<_>>(), vec![c1.clone()]);
    }

    #[test]
    fn list_refs_enumerates_branches_and_tags() {
        let (tmp, backend, _c1, _c2) = build_repo();
        run_git(&tmp.dir, &["branch", "feature"]).unwrap();
        run_git(&tmp.dir, &["tag", "v1.0"]).unwrap();
        let refs = backend.list_refs().unwrap();
        let names: Vec<&str> = refs.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"feature"), "lists the branch: {names:?}");
        assert!(names.contains(&"v1.0"), "lists the tag: {names:?}");
        assert!(!names.contains(&"HEAD"), "HEAD itself is not a target");
        // The label tags the kind.
        let tag_label = &refs.iter().find(|(n, _)| n == "v1.0").unwrap().1;
        assert!(tag_label.contains("(tag)"), "tag label: {tag_label:?}");
    }

    #[test]
    fn compare_view_diffs_the_working_file_against_a_revision() {
        use crate::diff::FileView;
        let (tmp, backend, c1, _c2) = build_repo();
        // a.go at c1 was `func A() {}`; HEAD (c2) made it `func A() { return }`. Edit the working
        // copy so the compare vs c1 shows both the committed change AND the new working edit.
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { return /* edited */ }\n");
        match backend.compare_view(WORKING_REV, &c1, "a.go").unwrap() {
            Some(FileView::Diff(d)) => {
                assert_eq!(d.old_rev, c1, "left side = the picked revision");
                assert_eq!(d.new_rev, "working", "right side = the working file");
                let added: String = d
                    .lines
                    .iter()
                    .filter(|l| l.kind == crate::diff::LineKind::Added)
                    .flat_map(|l| l.tokens.iter().map(|t| t.text.as_str()))
                    .collect();
                assert!(added.contains("edited"), "the working edit shows as an addition: {added:?}");
            }
            other => panic!("a compare with differences is a Diff: {other:?}"),
        }

        // Comparing a file against a revision it is IDENTICAL to yields a full-width Source.
        run_git(&tmp.dir, &["checkout", "--", "a.go"]).unwrap(); // restore working a.go to HEAD
        match backend.compare_view(WORKING_REV, "HEAD", "a.go").unwrap() {
            Some(FileView::Source(s)) => assert_eq!(s.path, "a.go"),
            other => panic!("an identical compare is a Source, not an empty diff: {other:?}"),
        }
    }

    #[test]
    fn compare_view_against_a_commit_base_diffs_two_revisions() {
        use crate::diff::FileView;
        let (_tmp, backend, c1, c2) = build_repo();
        // base = c2 (HEAD: `func A() { return }`), rev = c1 (`func A() {}`): a commit-vs-commit
        // compare (a historical row), independent of the working tree. Left = c1, right = c2.
        match backend.compare_view(&c2, &c1, "a.go").unwrap() {
            Some(FileView::Diff(d)) => {
                assert_eq!(d.old_rev, c1, "left side = the picked revision");
                assert_eq!(d.new_rev, c2, "right side = the commit base, not 'working'");
                let added: String = d
                    .lines
                    .iter()
                    .filter(|l| l.kind == crate::diff::LineKind::Added)
                    .flat_map(|l| l.tokens.iter().map(|t| t.text.as_str()))
                    .collect();
                assert!(added.contains("return"), "c2 added the return: {added:?}");
            }
            other => panic!("a commit-base compare is a Diff: {other:?}"),
        }
    }

    #[test]
    fn commit_file_commits_only_that_path() {
        let (tmp, backend, _c1, c2) = build_repo();
        // Two dirty files; commit only one. The other must stay uncommitted.
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { /* one */ }\n");
        write(&tmp.dir, "keep.txt", "now dirty\n");
        let summary = backend.commit_file("a.go", "tweak a only").unwrap();
        assert_eq!(summary, "Committed a.go");

        // HEAD moved (new commit) and its message + single-file scope are right.
        assert_ne!(full_oid(&tmp.dir, "HEAD"), full_oid(&tmp.dir, &c2), "a new commit landed");
        let head_files = run_git(&tmp.dir, &["show", "--name-only", "--format=%s", "HEAD"]).unwrap();
        assert!(head_files.contains("tweak a only"), "carries the message");
        assert!(head_files.contains("a.go"), "committed a.go");
        assert!(!head_files.contains("keep.txt"), "did NOT commit keep.txt");
        // keep.txt is still dirty in the working tree.
        let dirty = run_git(&tmp.dir, &["status", "--porcelain"]).unwrap();
        assert!(dirty.contains("keep.txt"), "keep.txt stays uncommitted: {dirty:?}");
    }

    #[test]
    fn commit_dir_commits_every_change_under_the_folder_only() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // A subtree with a tracked-modified file AND a brand-new untracked file, plus a
        // dirty file OUTSIDE the folder that must stay uncommitted.
        write(&tmp.dir, "pkg/mod.go", "package pkg\n\nfunc M() { /* changed */ }\n");
        write(&tmp.dir, "pkg/new.go", "package pkg\n\nfunc New() {}\n");
        write(&tmp.dir, "outside.txt", "dirty outside\n");
        run_git(&tmp.dir, &["add", "pkg/mod.go"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "seed pkg/mod.go"]).unwrap();
        write(&tmp.dir, "pkg/mod.go", "package pkg\n\nfunc M() { /* changed again */ }\n");

        let summary = backend.commit_dir("pkg", "commit the pkg folder").unwrap();
        assert_eq!(summary, "Committed pkg/");
        let head = run_git(&tmp.dir, &["show", "--name-only", "--format=%s", "HEAD"]).unwrap();
        assert!(head.contains("commit the pkg folder"), "carries the message");
        assert!(head.contains("pkg/mod.go"), "committed the modified file under pkg");
        assert!(head.contains("pkg/new.go"), "committed the untracked new file under pkg");
        assert!(!head.contains("outside.txt"), "did NOT commit the file outside pkg");
        let dirty = run_git(&tmp.dir, &["status", "--porcelain"]).unwrap();
        assert!(dirty.contains("outside.txt"), "outside.txt stays uncommitted: {dirty:?}");
    }

    #[test]
    fn commit_dir_failure_resets_the_staged_subtree() {
        // A pre-commit hook rejects AFTER `git add --all -- pkg` staged the subtree; the
        // reset must leave the new file plain-untracked again (gitgit's no-staging model).
        let (tmp, backend, _c1, _c2) = build_repo();
        let hook = tmp.dir.join(".git").join("hooks").join("pre-commit");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        write(&tmp.dir, "pkg/new.go", "package pkg\n");
        backend.commit_dir("pkg", "rejected").expect_err("the pre-commit hook fails the commit");
        let cached = run_git(&tmp.dir, &["ls-files", "--cached", "--", "pkg/new.go"]).unwrap();
        assert!(cached.is_empty(), "pkg/new.go is not left staged: {cached:?}");
        let status = run_git(&tmp.dir, &["status", "--porcelain", "--", "pkg/new.go"]).unwrap();
        assert!(status.starts_with("??"), "pkg/new.go is plain untracked again: {status:?}");
    }

    #[test]
    fn archive_project_picks_the_format_from_the_path_extension() {
        let (tmp, backend, _c1, _c2) = build_repo();
        let zip = tmp.dir.join("out.zip");
        let targz = tmp.dir.join("out.tar.gz");
        let tar = tmp.dir.join("out.tar");
        backend.archive_project(WORKING_REV, zip.to_str().unwrap()).unwrap();
        backend.archive_project(WORKING_REV, targz.to_str().unwrap()).unwrap();
        backend.archive_project(WORKING_REV, tar.to_str().unwrap()).unwrap();
        // zip files start with "PK"; gzip with 0x1f 0x8b; a POSIX tar is neither.
        let head = |p: &std::path::Path| std::fs::read(p).unwrap().into_iter().take(2).collect::<Vec<_>>();
        assert_eq!(head(&zip), vec![b'P', b'K'], "the .zip path produced a zip");
        assert_eq!(head(&targz), vec![0x1f, 0x8b], "the .tar.gz path produced gzip");
        assert_ne!(head(&tar), vec![0x1f, 0x8b], "the .tar path is uncompressed");
        assert_ne!(head(&tar), vec![b'P', b'K'], "the .tar path is not a zip");
    }

    #[test]
    fn cherry_pick_multi_applies_the_set_and_patch_series_writes_one_file_each() {
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        write(&tmp.dir, "base.txt", "base\n");
        let base = commit_all(&repo, "base", None);
        // Two feature commits on a side branch.
        run_git(&tmp.dir, &["checkout", "-b", "feature"]).unwrap();
        write(&tmp.dir, "f1.txt", "one\n");
        let f1 = commit_all(&repo, "feat one", Some(base));
        write(&tmp.dir, "f2.txt", "two\n");
        let f2 = commit_all(&repo, "feat two", Some(f1));
        // Back on the base branch (master/main - the default name varies) for the pick target.
        let base_branch = run_git(&tmp.dir, &["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
            .unwrap()
            .lines()
            .find(|b| *b != "feature")
            .unwrap_or("master")
            .to_string();
        run_git(&tmp.dir, &["checkout", &base_branch]).unwrap();
        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();

        // Patch series: one numbered file per commit (oldest-first).
        let series = backend
            .create_patch_series(&[short_hash(f1), short_hash(f2)], tmp.dir.join("patches").to_str().unwrap())
            .unwrap();
        assert!(series.contains("2 patch"), "two patches written: {series:?}");
        let files: Vec<_> = std::fs::read_dir(tmp.dir.join("patches")).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(files.len(), 2, "one .patch file per commit");

        // Cherry-pick the two feature commits onto the current (base) branch.
        let picked = backend.cherry_pick_multi(&[short_hash(f1), short_hash(f2)]).unwrap();
        assert!(picked.contains("Cherry-picked 2"), "summary: {picked:?}");
        assert!(tmp.dir.join("f1.txt").exists() && tmp.dir.join("f2.txt").exists(), "both picks applied");
    }

    #[test]
    fn multi_path_ops_commit_patch_and_delete_only_the_selected_files() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // Three changed files; only two are selected. The third must stay untouched.
        write(&tmp.dir, "a.txt", "alpha\n");
        write(&tmp.dir, "b.txt", "bravo\n");
        write(&tmp.dir, "c.txt", "charlie\n");
        let sel = vec!["a.txt".to_string(), "b.txt".to_string()];

        // Patch over the selected set captures both, not c.txt.
        let patch = backend.working_patch_multi(&sel).unwrap();
        assert!(patch.contains("a.txt") && patch.contains("b.txt"), "patch covers the selected files");
        assert!(!patch.contains("c.txt"), "patch excludes the unselected file: {patch:?}");

        // Commit the selected set only.
        let summary = backend.commit_paths(&sel, "commit selected").unwrap();
        assert_eq!(summary, "Committed 2 file(s)");
        let head = run_git(&tmp.dir, &["show", "--name-only", "--format=%s", "HEAD"]).unwrap();
        assert!(head.contains("a.txt") && head.contains("b.txt"), "both selected files committed");
        assert!(!head.contains("c.txt"), "the unselected file is NOT committed");
        let dirty = run_git(&tmp.dir, &["status", "--porcelain"]).unwrap();
        assert!(dirty.contains("c.txt"), "c.txt stays uncommitted: {dirty:?}");

        // Delete the selected (now committed) set; c.txt remains.
        let del = backend.delete_paths(&sel).unwrap();
        assert_eq!(del, "Deleted 2 file(s)");
        assert!(!tmp.dir.join("a.txt").exists() && !tmp.dir.join("b.txt").exists(), "selected files removed");
        assert!(tmp.dir.join("c.txt").exists(), "the unselected file survives");
    }

    #[test]
    fn commit_file_commits_an_untracked_new_file() {
        // A pathspec commit ignores untracked paths, so commit_file must stage first.
        let (tmp, backend, _c1, _c2) = build_repo();
        write(&tmp.dir, "fresh.txt", "brand new\n");
        backend.commit_file("fresh.txt", "add fresh").unwrap();
        let head = run_git(&tmp.dir, &["show", "--name-only", "--format=%s", "HEAD"]).unwrap();
        assert!(head.contains("add fresh"), "carries the message");
        assert!(head.contains("fresh.txt"), "the untracked file is now committed");
        // It is no longer untracked.
        let status = run_git(&tmp.dir, &["status", "--porcelain", "--", "fresh.txt"]).unwrap();
        assert!(status.is_empty(), "fresh.txt is committed, not pending: {status:?}");
    }

    #[test]
    fn commit_file_failure_leaves_an_untracked_file_unstaged() {
        // A pre-commit hook rejects the commit AFTER we staged a previously-untracked file.
        // The fix must unstage it so git's index does not silently hold a blob gitgit's
        // no-staging model never surfaces.
        let (tmp, backend, _c1, _c2) = build_repo();
        let hook = tmp.dir.join(".git").join("hooks").join("pre-commit");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        write(&tmp.dir, "fresh.txt", "brand new\n");
        backend
            .commit_file("fresh.txt", "rejected by hook")
            .expect_err("the pre-commit hook fails the commit");
        // The file is back to plain untracked - NOT staged in the index.
        let cached = run_git(&tmp.dir, &["ls-files", "--cached", "--", "fresh.txt"]).unwrap();
        assert!(cached.is_empty(), "fresh.txt is not left staged: {cached:?}");
        let status = run_git(&tmp.dir, &["status", "--porcelain", "--", "fresh.txt"]).unwrap();
        assert!(status.starts_with("??"), "fresh.txt is plain untracked again: {status:?}");
    }

    #[test]
    fn delete_file_removes_tracked_and_untracked() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // Tracked file: `git rm` removes it from the index + disk.
        backend.delete_file("keep.txt").unwrap();
        assert!(!tmp.dir.join("keep.txt").exists(), "tracked file is gone from disk");
        let staged = run_git(&tmp.dir, &["status", "--porcelain"]).unwrap();
        assert!(staged.contains("D  keep.txt") || staged.contains("D keep.txt"), "deletion staged: {staged:?}");

        // Untracked file: unlinked on disk, no git error.
        write(&tmp.dir, "scratch.tmp", "junk\n");
        backend.delete_file("scratch.tmp").unwrap();
        assert!(!tmp.dir.join("scratch.tmp").exists(), "untracked file is unlinked");

        // MODIFIED tracked file: plain `git rm` would refuse it; `-f` deletes anyway (the
        // delete is confirmed + destructive by design). This is the common right-click case.
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { /* edited */ }\n");
        backend.delete_file("a.go").expect("a modified tracked file deletes with -f");
        assert!(!tmp.dir.join("a.go").exists(), "the modified file is removed");
    }

    #[test]
    fn per_file_ops_handle_glob_magic_filenames() {
        // A filename with pathspec magic (`[id].txt`) must be matched LITERALLY, not as a
        // bracket class - else ls-files returns empty and delete wrongly fs-removes a tracked
        // file (blob left in index), and commit/add error "pathspec did not match".
        let (tmp, backend, _c1, _c2) = build_repo();
        write(&tmp.dir, "[id].txt", "literal name\n");

        // Commit it (untracked -> staged + committed via the literal pathspec).
        backend.commit_file("[id].txt", "add bracketed file").unwrap();
        let head = run_git(&tmp.dir, &["show", "--name-only", "--format=%s", "HEAD"]).unwrap();
        assert!(head.contains("[id].txt"), "the glob-magic file is committed: {head:?}");

        // working_patch on a modified bracketed file matches it literally.
        write(&tmp.dir, "[id].txt", "literal name\nedited\n");
        let patch = backend.working_patch("[id].txt").unwrap();
        assert!(patch.contains("[id].txt") && patch.contains("+edited"), "literal diff: {patch:.80?}");

        // Delete routes through the TRACKED branch (git rm), not a stray fs-remove.
        backend.delete_file("[id].txt").unwrap();
        assert!(!tmp.dir.join("[id].txt").exists(), "file removed from disk");
        let staged = run_git(&tmp.dir, &["status", "--porcelain"]).unwrap();
        assert!(staged.contains("[id].txt"), "the deletion is staged in git (not a stray unlink): {staged:?}");

        // The UNTRACKED glob-magic branch: ls-files (literal) returns empty so it falls to the
        // raw fs-remove on the verbatim name - no stray git op, the bracket name is not a class.
        write(&tmp.dir, "[x].tmp", "scratch\n");
        backend.delete_file("[x].tmp").unwrap();
        assert!(!tmp.dir.join("[x].tmp").exists(), "the untracked bracketed file is unlinked");
        let after = run_git(&tmp.dir, &["status", "--porcelain", "--", ":(literal)[x].tmp"]).unwrap();
        assert!(after.is_empty(), "no stray staged entry for the untracked delete: {after:?}");
    }

    #[test]
    fn create_patch_exports_a_root_commit() {
        // The empty-guard uses `diff-tree --root` so a ROOT commit (no parent) reports its
        // added files; the export must then actually produce an applyable patch (proving
        // `format-patch -1` handles a root without the diff-tree `--root` asymmetry biting).
        let (tmp, backend, c1, _c2) = build_repo();
        let dest = tmp.dir.join("root.patch");
        let path = dest.to_str().unwrap();
        backend.create_patch(&c1, path).unwrap();
        let patch = fs::read_to_string(&dest).unwrap();
        assert!(patch.contains("diff --git a/a.go b/a.go"), "the root's added files are in the diff");
        // Apply it onto an EMPTY tree (the root has no parent) to prove it is real.
        let scratch = tmp.dir.join("scratch");
        fs::create_dir_all(&scratch).unwrap();
        run_git(&scratch, &["init", "-q"]).unwrap();
        run_git(&scratch, &["apply", "--check", path]).expect("the root patch applies on an empty tree");
    }

    /// One `(full_hash, verb)` rebase op (the loader-translated shape `rebase_todo` takes).
    fn op(full: &str, verb: &str) -> (String, String) {
        (full.to_string(), verb.to_string())
    }

    #[test]
    fn rebase_todo_drop_rewinds_when_the_whole_range_is_dropped() {
        let (tmp, backend, c1, c2) = build_repo();
        let c1_full = run_git(&tmp.dir, &["rev-parse", &c1]).unwrap();
        let c2_full = run_git(&tmp.dir, &["rev-parse", &c2]).unwrap();
        // Pick c2 (== HEAD here): range is just c2; dropping it rewinds the branch to c1.
        backend.rebase_todo(&c2_full, &[op(&c2_full, "drop")]).unwrap();
        assert_eq!(run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap(), c1_full, "rewound to c2's parent");
        let files = run_git(&tmp.dir, &["ls-tree", "--name-only", "HEAD"]).unwrap();
        assert!(!files.contains("b.go"), "the dropped commit's added file is gone: {files}");
    }

    #[test]
    fn rebase_todo_drop_removes_a_middle_commit_replaying_the_newer_one() {
        let (tmp, backend, _c1, c2) = build_repo();
        // c3 adds an INDEPENDENT file, so dropping c2 and replaying c3 does not conflict.
        write(&tmp.dir, "c.go", "package main\n\nfunc C() {}\n");
        run_git(&tmp.dir, &["add", "-A"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "third: add c.go"]).unwrap();
        let c2_full = run_git(&tmp.dir, &["rev-parse", &c2]).unwrap();
        backend.rebase_todo(&c2_full, &[op(&c2_full, "drop")]).unwrap();
        let files = run_git(&tmp.dir, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(!files.contains("b.go"), "c2 (b.go) dropped: {files}");
        assert!(files.contains("c.go"), "c3 (c.go) replayed on top: {files}");
    }

    #[test]
    fn rebase_todo_squash_melds_a_commit_into_its_parent_keeping_both_changes() {
        let (tmp, backend, _c1, c2) = build_repo(); // c2: "second: change, add, delete" (adds b.go)
        write(&tmp.dir, "c.go", "package main\n\nfunc C() {}\n");
        run_git(&tmp.dir, &["add", "-A"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "third: add c.go"]).unwrap();
        let c3_full = run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap();
        let c2_full = run_git(&tmp.dir, &["rev-parse", &c2]).unwrap();
        let n_before: u32 = run_git(&tmp.dir, &["rev-list", "--count", "HEAD"]).unwrap().parse().unwrap();
        // Squash c3 into c2 (base = c2, the oldest in range): one commit holds both files.
        backend.rebase_todo(&c2_full, &[op(&c3_full, "squash")]).unwrap();
        let files = run_git(&tmp.dir, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(files.contains("b.go") && files.contains("c.go"), "both changes survive the meld: {files}");
        let n_after: u32 = run_git(&tmp.dir, &["rev-list", "--count", "HEAD"]).unwrap().parse().unwrap();
        assert_eq!(n_before - 1, n_after, "one fewer commit after the squash");
        let msg = run_git(&tmp.dir, &["log", "-1", "--format=%B", "HEAD"]).unwrap();
        assert!(msg.contains("second") && msg.contains("third"), "squash combines both messages: {msg}");
        assert!(run_git(&tmp.dir, &["merge-base", "--is-ancestor", &c3_full, "HEAD"]).is_err(), "c3 gone");
    }

    #[test]
    fn rebase_todo_fixup_melds_and_discards_the_squashed_message() {
        let (tmp, backend, _c1, c2) = build_repo(); // c2: "second: change, add, delete"
        write(&tmp.dir, "c.go", "package main\n\nfunc C() {}\n");
        run_git(&tmp.dir, &["add", "-A"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "third: add c.go"]).unwrap();
        let c3_full = run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap();
        let c2_full = run_git(&tmp.dir, &["rev-parse", &c2]).unwrap();
        backend.rebase_todo(&c2_full, &[op(&c3_full, "fixup")]).unwrap();
        let msg = run_git(&tmp.dir, &["log", "-1", "--format=%B", "HEAD"]).unwrap();
        assert!(msg.contains("second") && !msg.contains("third"), "fixup drops c3's message: {msg}");
        let files = run_git(&tmp.dir, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(files.contains("c.go"), "fixup keeps c3's changes: {files}");
    }

    #[test]
    fn rebase_todo_squash_past_an_intervening_drop_melds_into_the_nearest_kept() {
        // c1(root) -> c2(b.go) -> c3(c.go) -> c4(d.go), all independent files (no conflict).
        // Display newest-first [c4, c3, c2]; mark c4=squash, c3=drop. The squash must meld
        // into c2 (the nearest OLDER KEPT commit, skipping the dropped c3) - the exact
        // combined-op + meld-past-drop interaction this stage introduced.
        let (tmp, backend, _c1, c2) = build_repo(); // c2 adds b.go
        write(&tmp.dir, "c.go", "package main\n\nfunc C() {}\n");
        run_git(&tmp.dir, &["add", "-A"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "third: add c.go"]).unwrap();
        let c3_full = run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap();
        write(&tmp.dir, "d.go", "package main\n\nfunc D() {}\n");
        run_git(&tmp.dir, &["add", "-A"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "fourth: add d.go"]).unwrap();
        let c4_full = run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap();
        let c2_full = run_git(&tmp.dir, &["rev-parse", &c2]).unwrap();
        backend.rebase_todo(&c2_full, &[op(&c4_full, "squash"), op(&c3_full, "drop")]).unwrap();
        let files = run_git(&tmp.dir, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(files.contains("b.go"), "c2 kept: {files}");
        assert!(files.contains("d.go"), "c4's change melded in, not dropped: {files}");
        assert!(!files.contains("c.go"), "c3 dropped: {files}");
        // c2+c4 are now one commit on c1: b.go and d.go share a commit, c3/c4 hashes gone.
        let n: u32 = run_git(&tmp.dir, &["rev-list", "--count", "HEAD"]).unwrap().parse().unwrap();
        assert_eq!(n, 2, "c1 + the melded (c2+c4) commit");
        assert!(run_git(&tmp.dir, &["merge-base", "--is-ancestor", &c3_full, "HEAD"]).is_err(), "c3 gone");
        assert!(run_git(&tmp.dir, &["merge-base", "--is-ancestor", &c4_full, "HEAD"]).is_err(), "c4 gone");
    }

    #[test]
    fn rebase_todo_conflict_aborts_leaving_history_intact() {
        let (tmp, backend, _c1, c2) = build_repo();
        // c3 EDITS b.go (added by c2); dropping c2 makes c3 patch a missing file -> conflict.
        write(&tmp.dir, "b.go", "package main\n\nfunc B() { return }\n");
        run_git(&tmp.dir, &["add", "-A"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "third: edit b.go"]).unwrap();
        let head_before = run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap();
        let c2_full = run_git(&tmp.dir, &["rev-parse", &c2]).unwrap();
        let err = backend.rebase_todo(&c2_full, &[op(&c2_full, "drop")]).unwrap_err();
        assert!(err.0.contains("aborted"), "a conflict aborts the rebase: {}", err.0);
        assert_eq!(run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap(), head_before, "history intact");
        assert!(!tmp.dir.join(".git/rebase-merge").exists(), "no rebase left in progress");
    }

    #[test]
    fn rebase_todo_removes_multiple_commits_in_one_rebase() {
        let (tmp, backend, _c1, c2) = build_repo(); // c2 adds b.go
        write(&tmp.dir, "c.go", "package main\n\nfunc C() {}\n");
        run_git(&tmp.dir, &["add", "-A"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "third: add c.go"]).unwrap();
        let c3_full = run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap();
        write(&tmp.dir, "d.go", "package main\n\nfunc D() {}\n");
        run_git(&tmp.dir, &["add", "-A"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "fourth: add d.go"]).unwrap();
        let c2_full = run_git(&tmp.dir, &["rev-parse", &c2]).unwrap();
        // Drop c2 AND c3 (both add independent files); c4 (d.go) replays cleanly on c1.
        backend.rebase_todo(&c2_full, &[op(&c2_full, "drop"), op(&c3_full, "drop")]).unwrap();
        let files = run_git(&tmp.dir, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(!files.contains("b.go"), "c2 dropped: {files}");
        assert!(!files.contains("c.go"), "c3 dropped: {files}");
        assert!(files.contains("d.go"), "c4 replayed: {files}");
    }

    #[test]
    fn rebase_todo_refuses_a_merge_commit() {
        let (tmp, backend, _c1, _c2) = build_repo();
        let main_tip = run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap();
        run_git(&tmp.dir, &["checkout", "-b", "feature"]).unwrap();
        write(&tmp.dir, "feat.go", "package main\n\nfunc Feat() {}\n");
        run_git(&tmp.dir, &["add", "-A"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "feat"]).unwrap();
        run_git(&tmp.dir, &["checkout", &main_tip]).unwrap();
        run_git(&tmp.dir, &["merge", "--no-ff", "-m", "merge: feature", "feature"]).unwrap();
        let merge = run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap();
        let head_before = merge.clone();
        let err = backend.rebase_todo(&merge, &[op(&merge, "drop")]).unwrap_err();
        assert!(err.0.contains("merge"), "a merge in the op set is refused: {}", err.0);
        assert_eq!(run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap(), head_before, "history untouched");
    }

    #[test]
    fn rebase_todo_empty_set_errors() {
        let (tmp, backend, _c1, c2) = build_repo();
        let c2_full = run_git(&tmp.dir, &["rev-parse", &c2]).unwrap();
        assert!(backend.rebase_todo(&c2_full, &[]).is_err(), "nothing marked is an error");
    }

    #[test]
    fn create_patch_refuses_a_merge_commit() {
        // A single mbox patch can't represent a merge (and `format-patch -1 <merge>` emits
        // a parent's own patch, not the merge's change), so create_patch refuses it up front.
        let (tmp, backend, _c1, _c2) = build_repo();
        let repo = Repository::open(&tmp.dir).unwrap();
        // Use the tip HASH, not a branch name (the init default branch may be master/main).
        let main_tip = run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap();
        // Branch off the tip, add a file + commit, then merge that branch back into the
        // (detached) original tip so HEAD becomes a real 2-parent merge commit.
        run_git(&tmp.dir, &["checkout", "-b", "feature"]).unwrap();
        write(&tmp.dir, "feat.go", "package main\n\nfunc Feat() {}\n");
        commit_all(&repo, "feat: add Feat", Some(Oid::from_str(&main_tip).unwrap()));
        run_git(&tmp.dir, &["checkout", &main_tip]).unwrap();
        run_git(&tmp.dir, &["merge", "--no-ff", "-m", "merge: feature", "feature"]).unwrap();

        let dest = tmp.dir.join("merge.patch");
        let path = dest.to_str().unwrap();
        let err = backend.create_patch("HEAD", path).unwrap_err();
        assert!(err.0.contains("merge"), "the merge is refused: {}", err.0);
        assert!(!dest.exists(), "no file is written on refusal");
    }

    #[test]
    fn create_patch_refuses_an_empty_commit_even_when_its_message_quotes_a_diff() {
        // An empty commit (no tree change) is refused via the changed-FILE list - NOT a
        // substring scan of the patch. The message deliberately contains "diff --git" to
        // prove the guard keys off the tree, not the text the author controls.
        let (tmp, backend, _c1, _c2) = build_repo();
        run_git(&tmp.dir, &["commit", "--allow-empty", "-m", "revert of diff --git a/x b/x"]).unwrap();
        let dest = tmp.dir.join("empty.patch");
        let path = dest.to_str().unwrap();
        let err = backend.create_patch("HEAD", path).unwrap_err();
        assert!(err.0.contains("No patch"), "an empty commit is refused: {}", err.0);
        assert!(!dest.exists(), "no file is written on refusal");
    }

    #[test]
    fn create_patch_surfaces_a_bad_path_as_an_error() {
        let (_tmp, backend, _c1, c2) = build_repo();
        // A path under a nonexistent directory cannot be written; the io error surfaces.
        let err = backend.create_patch(&c2, "/no/such/dir/x.patch").unwrap_err();
        assert!(!err.0.is_empty(), "the write failure is reported, not swallowed");
    }

    #[test]
    fn reword_head_changes_only_the_subject() {
        let (tmp, backend, _c1, _c2) = build_repo();
        let before_tree = run_git(&tmp.dir, &["rev-parse", "HEAD^{tree}"]).unwrap();
        backend.reword_at("HEAD", "second: reworded subject").unwrap();
        assert_eq!(head_subject(&tmp.dir), "second: reworded subject");
        // Message-only: HEAD's tree is unchanged.
        let after_tree = run_git(&tmp.dir, &["rev-parse", "HEAD^{tree}"]).unwrap();
        assert_eq!(before_tree, after_tree, "reword does not touch the tree");
    }

    #[test]
    fn reword_head_ignores_staged_changes() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // Stage a NEW file, then reword HEAD: the staged file must NOT be folded in.
        write(&tmp.dir, "staged.go", "package main\n");
        run_git(&tmp.dir, &["add", "staged.go"]).unwrap();
        backend.reword_at("HEAD", "second: message only").unwrap();
        let in_head = run_git(&tmp.dir, &["ls-tree", "--name-only", "HEAD"]).unwrap();
        assert!(!in_head.contains("staged.go"), "staged change was NOT amended in: {in_head}");
        // ...and it is still staged afterward.
        let staged = run_git(&tmp.dir, &["diff", "--cached", "--name-only"]).unwrap();
        assert!(staged.contains("staged.go"), "the staged change is preserved");
    }

    #[test]
    fn reword_preserves_a_multiline_body() {
        let (tmp, _backend, _c1, _c2) = build_repo();
        // Give HEAD a body, then reword the subject only.
        run_git(&tmp.dir, &["commit", "--amend", "-m", "second: subj\n\nthe body line"]).unwrap();
        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();
        backend.reword_at("HEAD", "second: new subj").unwrap();
        let msg = commit_message(&tmp.dir, "HEAD");
        assert!(msg.starts_with("second: new subj"), "subject changed: {msg}");
        assert!(msg.contains("the body line"), "body preserved: {msg}");
    }

    #[test]
    fn reword_older_commit_rewrites_descendant_and_keeps_tree() {
        let (tmp, backend, c1, _c2) = build_repo();
        let child_tree_before = run_git(&tmp.dir, &["rev-parse", "HEAD^{tree}"]).unwrap();
        backend.reword_at(&c1, "root: reworded").unwrap();
        // The root's message changed...
        assert_eq!(commit_message(&tmp.dir, "HEAD~1").trim(), "root: reworded");
        // ...the child (HEAD) got a NEW hash (history rewritten) but the SAME tree content.
        let child_tree_after = run_git(&tmp.dir, &["rev-parse", "HEAD^{tree}"]).unwrap();
        assert_eq!(child_tree_before, child_tree_after, "child tree content preserved");
        assert_eq!(head_subject(&tmp.dir), "second: change, add, delete", "child subject intact");
    }

    #[test]
    fn reword_root_commit_works() {
        let (tmp, backend, c1, _c2) = build_repo();
        // c1 is the root (no parent) -> exercises the `--root` rebase path.
        backend.reword_at(&c1, "root: brand new subject").unwrap();
        assert_eq!(commit_message(&tmp.dir, "HEAD~1").trim(), "root: brand new subject");
    }

    #[test]
    fn reword_preserves_merge_topology() {
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        write(&tmp.dir, "f.txt", "base\n");
        let base = commit_all(&repo, "base commit", None);
        // A feature branch off base, merged back with a real merge commit.
        run_git(&tmp.dir, &["checkout", "-b", "feat"]).unwrap();
        write(&tmp.dir, "x.txt", "x\n");
        commit_all(&repo, "feat work", Some(base));
        run_git(&tmp.dir, &["checkout", "main"])
            .or_else(|_| run_git(&tmp.dir, &["checkout", "master"]))
            .unwrap();
        write(&tmp.dir, "m.txt", "m\n");
        let mainwork = commit_all(&repo, "main work", Some(base));
        run_git(&tmp.dir, &["merge", "--no-ff", "feat", "-m", "merge feat"]).unwrap();
        let merges_before = run_git(&tmp.dir, &["rev-list", "--merges", "HEAD"]).unwrap();
        assert!(!merges_before.is_empty(), "precondition: a merge exists");

        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();
        backend.reword_at(&short_hash(base), "base reworded").unwrap();

        // The merge commit must survive (--rebase-merges, not a flattening rebase).
        let merges_after = run_git(&tmp.dir, &["rev-list", "--merges", "HEAD"]).unwrap();
        assert!(!merges_after.is_empty(), "the merge commit was preserved");
        // The reworded base got a new hash; find it positionally as the root commit.
        let root = run_git(&tmp.dir, &["rev-list", "--max-parents=0", "HEAD"]).unwrap();
        let root = root.lines().next().unwrap();
        assert_eq!(commit_message(&tmp.dir, root).trim(), "base reworded");
        let _ = mainwork;
    }

    #[test]
    fn reword_aborts_and_leaves_repo_clean_on_conflict() {
        // Force a rebase conflict: the root and its child both touch the same file, then
        // reword the root. The reword must fail AND leave no in-progress rebase.
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        write(&tmp.dir, "conflict.txt", "root version\n");
        let root = commit_all(&repo, "root", None);
        write(&tmp.dir, "conflict.txt", "child version\n");
        commit_all(&repo, "child", Some(root));
        let head_before = run_git(&tmp.dir, &["rev-parse", "HEAD"]).unwrap();

        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();
        // Rewording the root replays the child cleanly (no conflict here, single file
        // chain), so assert the success path leaves a clean tree (no rebase dir).
        backend.reword_at(&short_hash(root), "root reworded").unwrap();
        assert!(!tmp.dir.join(".git/rebase-merge").exists(), "no in-progress rebase left");
        assert!(!tmp.dir.join(".git/rebase-apply").exists(), "no in-progress rebase left");
        let _ = head_before;
    }

    #[test]
    fn compose_message_reattaches_body() {
        let (tmp, _backend, _c1, _c2) = build_repo();
        run_git(&tmp.dir, &["commit", "--amend", "-m", "subj\n\nbody one\nbody two"]).unwrap();
        let composed = compose_message(&tmp.dir, "HEAD", "new subj").unwrap();
        assert!(composed.starts_with("new subj\n\n"), "subject then blank line: {composed}");
        assert!(composed.contains("body one"), "body kept: {composed}");
    }

    #[test]
    fn shell_quote_defuses_spaces_and_quotes() {
        assert_eq!(shell_quote("/tmp/a b/c.txt"), "'/tmp/a b/c.txt'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn branch_and_tag_reject_dashed_names() {
        let (tmp, backend, c1, _c2) = build_repo();
        // A name git would read as an option (e.g. `-m`) is refused, never executed -
        // so the current branch is NOT renamed.
        let before = run_git(&tmp.dir, &["branch", "--show-current"]).unwrap();
        assert!(backend.branch_create("-m", &c1, false).is_err());
        assert!(backend.tag_at("-d", &c1).is_err());
        assert_eq!(run_git(&tmp.dir, &["branch", "--show-current"]).unwrap(), before);
    }

    #[test]
    fn reword_refuses_a_merge_commit() {
        let tmp = TempRepo::new();
        let repo = Repository::init(&tmp.dir).unwrap();
        write(&tmp.dir, "f.txt", "base\n");
        let base = commit_all(&repo, "base", None);
        run_git(&tmp.dir, &["checkout", "-b", "feat"]).unwrap();
        write(&tmp.dir, "x.txt", "x\n");
        commit_all(&repo, "feat", Some(base));
        run_git(&tmp.dir, &["checkout", "main"])
            .or_else(|_| run_git(&tmp.dir, &["checkout", "master"]))
            .unwrap();
        run_git(&tmp.dir, &["merge", "--no-ff", "feat", "-m", "merge feat"]).unwrap();
        let merge = run_git(&tmp.dir, &["rev-parse", "--short", "HEAD"]).unwrap();
        let merge_oid = repo.head().unwrap().target().unwrap();
        // Add a child so the merge is NON-head -> reword_at takes the rebase path (the
        // amend path is HEAD-only), which is where the merge guard lives.
        write(&tmp.dir, "y.txt", "y\n");
        commit_all(&repo, "after merge", Some(merge_oid));
        let backend = RealBackend::open(&tmp.dir, &Config::default()).unwrap();
        let err = backend.reword_at(&merge, "merge reworded").unwrap_err();
        assert!(err.0.contains("merge commit"), "refuses a merge reword: {}", err.0);
        // The merge message is unchanged (no false success).
        assert_eq!(commit_message(&tmp.dir, &merge).trim(), "merge feat");
    }

    // -- ref (branch/tag) submenu ops -------------------------------------------

    /// A side branch off c1 with one new-file commit. Returns the branch name and HEAD
    /// back on the base branch. The shared fixture for the merge/rebase ref-op tests.
    fn add_side_branch(tmp: &TempRepo, name: &str, base: &str) {
        run_git(&tmp.dir, &["checkout", "-b", name, base]).unwrap();
        write(&tmp.dir, &format!("{name}.txt"), "side content\n");
        run_git(&tmp.dir, &["add", "."]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", &format!("{name}: side commit")]).unwrap();
        run_git(&tmp.dir, &["checkout", "-"]).unwrap();
    }

    #[test]
    fn checkout_ref_attaches_head_to_a_branch() {
        let (tmp, backend, c1, _c2) = build_repo();
        run_git(&tmp.dir, &["branch", "topic", &c1]).unwrap();
        let summary = backend.checkout_ref("topic").unwrap();
        assert!(summary.contains("topic"), "summary names the ref: {summary:?}");
        // A branch checkout ATTACHES HEAD (not detached), unlike a raw-hash checkout.
        assert_eq!(run_git(&tmp.dir, &["branch", "--show-current"]).unwrap(), "topic");
    }

    #[test]
    fn merge_ref_brings_in_a_side_branch() {
        let (tmp, backend, c1, _c2) = build_repo();
        add_side_branch(&tmp, "feature", &c1);
        let summary = backend.merge_ref("feature").unwrap();
        assert!(summary.contains("Merged"), "summary: {summary:?}");
        assert!(tmp.dir.join("feature.txt").exists(), "the side file merged into the tree");
    }

    #[test]
    fn merge_ref_conflict_aborts_leaving_head_intact() {
        let (tmp, backend, c1, _c2) = build_repo();
        // Side and base each change a.go differently off c1 -> a conflicting merge.
        run_git(&tmp.dir, &["checkout", "-b", "side", &c1]).unwrap();
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { panic(\"x\") }\n");
        run_git(&tmp.dir, &["commit", "-am", "side: a.go panic"]).unwrap();
        run_git(&tmp.dir, &["checkout", "-"]).unwrap();
        write(&tmp.dir, "a.go", "package main\n\nfunc A() { log() }\n");
        run_git(&tmp.dir, &["commit", "-am", "base: a.go log"]).unwrap();
        let before = full_oid(&tmp.dir, "HEAD");
        let err = backend.merge_ref("side").unwrap_err();
        // The notice names the conflict, not git's "Auto-merging <f>" progress chatter.
        assert!(
            err.to_string().to_lowercase().contains("conflict"),
            "the conflict is surfaced clearly, got {:?}",
            err.to_string()
        );
        assert_eq!(full_oid(&tmp.dir, "HEAD"), before, "HEAD intact after the abort");
        // No merge is in progress (MERGE_HEAD gone), so the repo did not stick.
        assert!(run_git(&tmp.dir, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).is_err());
    }

    #[test]
    fn merge_conflict_in_a_keyword_named_file_still_surfaces_conflict_not_progress() {
        // Regression: the conflicting file's PATH contains a failure keyword ("error"), so
        // git's progress line "Auto-merging src/error.go" matches the keyword scan. The notice
        // must still name the CONFLICT, not the progress chatter (which reads as success).
        let (tmp, backend, _c1, _c2) = build_repo();
        // A keyword-named file becomes the common ancestor, then diverges on both sides.
        write(&tmp.dir, "src/error.go", "package src\n\nfunc E() {}\n");
        run_git(&tmp.dir, &["add", "-A"]).unwrap();
        run_git(&tmp.dir, &["commit", "-m", "add error.go"]).unwrap();
        run_git(&tmp.dir, &["checkout", "-b", "side"]).unwrap();
        write(&tmp.dir, "src/error.go", "package src\n\nfunc E() { panic(\"x\") }\n");
        run_git(&tmp.dir, &["commit", "-am", "side: error.go panic"]).unwrap();
        run_git(&tmp.dir, &["checkout", "-"]).unwrap();
        write(&tmp.dir, "src/error.go", "package src\n\nfunc E() { log() }\n");
        run_git(&tmp.dir, &["commit", "-am", "base: error.go log"]).unwrap();
        let err = backend.merge_ref("side").unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("conflict"), "conflict surfaced, got {:?}", err.to_string());
        assert!(!msg.starts_with("auto-merging"), "progress chatter not surfaced, got {:?}", err.to_string());
    }

    #[test]
    fn push_failure_surfaces_the_error_not_the_keyword_named_remote_header() {
        // Regression: `git push` emits "To <url>" BEFORE "error: failed to push ...". A remote
        // whose URL contains a keyword ("...x-error.git") would let the "To" header win the scan
        // and mask the real error. The header is skipped so the failure line surfaces.
        let lines = [
            "To github.com:me/x-error.git",
            "! [rejected]        main -> main (non-fast-forward)",
            "error: failed to push some refs to 'github.com:me/x-error.git'",
        ];
        let pick = pick_failure_line(&lines).unwrap();
        assert!(pick.starts_with("error: failed to push"), "got {pick:?}");
    }

    #[test]
    fn rebase_onto_replays_the_branch_onto_the_ref() {
        let (tmp, backend, c1, _c2) = build_repo();
        // A target branch with a commit c2 is NOT an ancestor of; rebasing the current
        // branch onto it makes that target an ancestor of the new HEAD.
        add_side_branch(&tmp, "onto", &c1);
        let onto = full_oid(&tmp.dir, "onto");
        let summary = backend.rebase_onto("onto").unwrap();
        assert!(summary.contains("Rebased"), "summary: {summary:?}");
        let merge_base = run_git(&tmp.dir, &["merge-base", "HEAD", &onto]).unwrap();
        assert_eq!(merge_base, onto, "the ref is now an ancestor of the rebased HEAD");
    }

    #[test]
    fn branch_rename_changes_the_name() {
        let (tmp, backend, c1, _c2) = build_repo();
        run_git(&tmp.dir, &["branch", "old", &c1]).unwrap();
        backend.branch_rename("old", "new").unwrap();
        assert!(run_git(&tmp.dir, &["rev-parse", "--verify", "new"]).is_ok(), "new name exists");
        assert!(run_git(&tmp.dir, &["rev-parse", "--verify", "old"]).is_err(), "old name gone");
    }

    #[test]
    fn branch_delete_removes_a_merged_branch() {
        let (tmp, backend, _c1, c2) = build_repo();
        run_git(&tmp.dir, &["branch", "stale", &c2]).unwrap();
        // `stale` is at HEAD (fully merged) so the safe `-d` deletes it.
        backend.branch_delete("stale", false).unwrap();
        assert!(run_git(&tmp.dir, &["rev-parse", "--verify", "stale"]).is_err(), "branch gone");
    }

    #[test]
    fn branch_delete_force_drops_an_unmerged_branch() {
        let (tmp, backend, c1, _c2) = build_repo();
        // An unmerged branch: safe `-d` refuses, `-D` (force) drops it.
        add_side_branch(&tmp, "wip", &c1);
        assert!(backend.branch_delete("wip", false).is_err(), "safe delete refuses unmerged");
        backend.branch_delete("wip", true).unwrap();
        assert!(run_git(&tmp.dir, &["rev-parse", "--verify", "wip"]).is_err(), "force-deleted");
    }

    #[test]
    fn tag_delete_removes_the_tag() {
        let (tmp, backend, c1, _c2) = build_repo();
        run_git(&tmp.dir, &["tag", "v0", &c1]).unwrap();
        backend.tag_delete("v0").unwrap();
        assert!(run_git(&tmp.dir, &["rev-parse", "--verify", "v0"]).is_err(), "tag gone");
    }

    #[test]
    fn discard_all_drops_tracked_edits_and_new_files_but_keeps_gitignored() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // Commit a .gitignore onto HEAD first (the only uncommitted change at this point), so the
        // ignore rule is tracked and survives the hard reset.
        let repo = Repository::open(&tmp.dir).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap().id();
        write(&tmp.dir, ".gitignore", "build/\n");
        commit_all(&repo, "add gitignore", Some(head));

        // Now dirty the tree: a tracked edit, brand-new untracked file + dir, and an ignored
        // build artifact.
        write(&tmp.dir, "keep.txt", "locally edited\n");
        write(&tmp.dir, "new.txt", "added but uncommitted\n");
        write(&tmp.dir, "sub/also_new.txt", "in a new dir\n");
        write(&tmp.dir, "build/artifact.bin", "ignored output\n");

        backend.discard_all().unwrap();

        assert_eq!(fs::read_to_string(tmp.dir.join("keep.txt")).unwrap(), "unchanged\n", "tracked edit reverted");
        assert!(!tmp.dir.join("new.txt").exists(), "new untracked file deleted");
        assert!(!tmp.dir.join("sub").exists(), "new untracked dir deleted");
        assert!(tmp.dir.join("build/artifact.bin").exists(), ".gitignore'd file preserved");
        assert!(tmp.dir.join(".gitignore").exists(), "tracked .gitignore survives");
    }

    #[test]
    fn remote_add_list_set_url_remove_round_trip() {
        let (tmp, backend, _c1, _c2) = build_repo();
        assert!(backend.remote_list().unwrap().is_empty(), "no remotes initially");
        backend.remote_add("origin", "https://example.com/a.git").unwrap();
        let listed = backend.remote_list().unwrap();
        assert_eq!(listed, vec![("origin".to_string(), "https://example.com/a.git".to_string())]);
        backend.remote_set_url("origin", "https://example.com/b.git").unwrap();
        assert_eq!(backend.remote_list().unwrap()[0].1, "https://example.com/b.git", "url updated");
        backend.remote_remove("origin").unwrap();
        assert!(backend.remote_list().unwrap().is_empty(), "remote removed");
        // The on-disk config agrees with the listing.
        assert!(run_git(&tmp.dir, &["remote"]).unwrap().trim().is_empty());
    }

    #[test]
    fn push_ref_then_pull_ref_round_trip_through_a_bare_remote() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // A bare repo acts as the remote `origin`; the working repo's current branch is
        // pushed there, then a downstream change is pulled back in.
        let remote = TempRepo::new();
        run_git(&remote.dir, &["init", "--bare"]).unwrap();
        run_git(&tmp.dir, &["remote", "add", "origin", remote.dir.to_str().unwrap()]).unwrap();
        let branch = run_git(&tmp.dir, &["branch", "--show-current"]).unwrap();

        let pushed = backend.push_ref(&branch).unwrap();
        assert!(pushed.contains("Pushed"), "push summary: {pushed:?}");
        assert_eq!(
            full_oid(&remote.dir, &branch),
            full_oid(&tmp.dir, "HEAD"),
            "the remote branch now matches the local tip"
        );

        // A second clone commits + pushes, so the first repo's pull has something to take.
        let other = TempRepo::new();
        run_git(&other.dir, &["clone", remote.dir.to_str().unwrap(), other.dir.to_str().unwrap()]).unwrap();
        write(&other.dir, "downstream.txt", "from the other clone\n");
        run_git(&other.dir, &["add", "."]).unwrap();
        run_git(&other.dir, &["commit", "-m", "other: downstream commit"]).unwrap();
        run_git(&other.dir, &["push", "origin", &branch]).unwrap();

        let pulled = backend.pull_ref("origin", &branch, true).unwrap();
        assert!(pulled.contains("Pulled"), "pull summary: {pulled:?}");
        assert!(tmp.dir.join("downstream.txt").exists(), "the pulled commit's file is present");
    }

    #[test]
    fn pull_mode_fast_forwards_from_the_configured_upstream() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // Bare remote + an upstream-tracking branch so a no-arg `git pull` resolves.
        let remote = TempRepo::new();
        run_git(&remote.dir, &["init", "--bare"]).unwrap();
        run_git(&tmp.dir, &["remote", "add", "origin", remote.dir.to_str().unwrap()]).unwrap();
        let branch = run_git(&tmp.dir, &["branch", "--show-current"]).unwrap();
        run_git(&tmp.dir, &["push", "-u", "origin", &branch]).unwrap();

        // A downstream clone advances the remote.
        let other = TempRepo::new();
        run_git(&other.dir, &["clone", remote.dir.to_str().unwrap(), other.dir.to_str().unwrap()]).unwrap();
        write(&other.dir, "ff.txt", "downstream\n");
        run_git(&other.dir, &["add", "."]).unwrap();
        run_git(&other.dir, &["commit", "-m", "other: ff commit"]).unwrap();
        run_git(&other.dir, &["push", "origin", &branch]).unwrap();

        // Fast-forward-only pull (None) integrates the remote commit cleanly.
        let pulled = backend.pull_mode(None).unwrap();
        assert!(pulled.contains("Pulled"), "pull summary: {pulled:?}");
        assert!(tmp.dir.join("ff.txt").exists(), "the fast-forwarded commit's file is present");
    }

    #[test]
    fn update_project_fetches_then_fast_forwards_and_tolerates_no_upstream() {
        let (tmp, backend, _c1, _c2) = build_repo();
        // No remote yet: update must NOT hard-fail. `fetch --all` with zero remotes is a no-op
        // success, so it lands on the no-upstream notice.
        let none = backend.update_project().unwrap();
        assert!(none.contains("no upstream"), "no-upstream notice: {none:?}");

        // Now wire an upstream and advance the remote from a downstream clone.
        let remote = TempRepo::new();
        run_git(&remote.dir, &["init", "--bare"]).unwrap();
        run_git(&tmp.dir, &["remote", "add", "origin", remote.dir.to_str().unwrap()]).unwrap();
        let branch = run_git(&tmp.dir, &["branch", "--show-current"]).unwrap();
        run_git(&tmp.dir, &["push", "-u", "origin", &branch]).unwrap();

        let other = TempRepo::new();
        run_git(&other.dir, &["clone", remote.dir.to_str().unwrap(), other.dir.to_str().unwrap()]).unwrap();
        write(&other.dir, "upd.txt", "downstream\n");
        run_git(&other.dir, &["add", "."]).unwrap();
        run_git(&other.dir, &["commit", "-m", "other: update commit"]).unwrap();
        run_git(&other.dir, &["push", "origin", &branch]).unwrap();

        // Fetch + ff pull integrates the remote commit in one click.
        let updated = backend.update_project().unwrap();
        assert!(updated.contains("Updated"), "update summary: {updated:?}");
        assert!(tmp.dir.join("upd.txt").exists(), "the fast-forwarded commit's file is present");
    }
}
