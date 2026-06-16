//! Domain model for the git log view.
//!
//! These types are deliberately backend-agnostic: today they are filled by the
//! fixture backend, later they will be filled by a real git backend (libgit2 /
//! gitoxide). The UI layer only ever sees these structs, so swapping the data
//! source is not a UI rewrite.

use fancy_regex::Regex;

use crate::diff::FileView;
use crate::graph_engine::{self, GraphLayout};
use crate::view_state::{FilterKind, ViewState};

/// Lifecycle of the repository data behind the view. A DOMAIN type (not a store
/// or runtime concern) so the PURE `ui` can read it to drive per-panel loading /
/// error states without importing `store`. `Loading` until the first
/// `RepoLoaded` arrives, `Ready` once data is in, `Error` on a backend failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Loading,
    Ready,
    Error(String),
    /// A transient success/hint line (e.g. "Reverted 3 files", or "Nothing
    /// selected to revert"). Rendered in the files toolbar's right margin; treated
    /// as `Ready` everywhere a loaded repo is concerned (it never gates a panel).
    Notice(String),
}

/// A person + timestamp pair (author or committer).
#[derive(Clone, Debug)]
pub struct Signature {
    pub name: String,
    pub email: String,
    /// Pre-formatted commit date ("DD.MM.YYYY, HH:MM" or ISO), as shown in the
    /// detail panel. The exact shape follows `[behavior].date_format`.
    pub when: String,
}

/// A branch/tag/remote label decorating a commit.
#[derive(Clone, Debug)]
pub struct Ref {
    pub name: String,
    pub kind: RefKind,
}

/// `Tag`/`Head` are unused by the current fixtures but complete the ref set the
/// backend will decorate commits with.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
    Head,
}

/// One styled span within a commit subject. The `tone` drives its color: plain text,
/// an inline hyperlink (blue), or a working-tree change count colored by file status
/// (the synthetic `<current>` row's `+N ~N -N` badges).
#[derive(Clone, Debug)]
pub struct SubjectSpan {
    pub text: String,
    pub tone: SubjectTone,
}

/// The color role of a [`SubjectSpan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SubjectTone {
    /// Ordinary subject text.
    #[default]
    Plain,
    /// An inline hyperlink (rendered in the link accent).
    Link,
    /// The accent-blue active marker - the dirty `<current>` row's label, so an
    /// uncommitted-changes row reads as live/actionable (the link-blue accent).
    Active,
    /// Dimmed subject text (the clean `<current>` row's label).
    Dim,
}

impl SubjectSpan {
    /// Plain (uncolored) subject text.
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), tone: SubjectTone::Plain }
    }
}

/// A single commit row in the log.
#[derive(Clone, Debug)]
pub struct Commit {
    pub hash: String,
    /// The full 40-char object id (the Copy picker's "full hash"). Empty for the
    /// synthetic working row; equals `hash` in fixtures/tests that need no real oid.
    pub full_hash: String,
    /// Short hashes of this commit's parents (first = mainline). A merge has 2+;
    /// the root has none. The graph engine derives the gutter topology from these.
    pub parents: Vec<String>,
    /// Subject split into spans so embedded URLs render as blue links.
    pub subject: Vec<SubjectSpan>,
    pub refs: Vec<Ref>,
    pub author: String,
    /// Pre-formatted absolute "DD.MM.YYYY, HH:MM" (or ISO) date. The stable string the
    /// Date filter parses and the detail panel shows; NOT what the log column renders.
    pub date: String,
    /// What the log column renders: a relative "Today, HH:MM" / "Yesterday, HH:MM" for
    /// recent commits, else the same absolute string as `date`. The real backend
    /// computes this against the load-time clock; fixtures set it equal to `date` so the
    /// golden render stays clock-independent.
    pub date_label: String,
    /// Author is the currently logged-in user -> rendered bold.
    pub is_me: bool,
    /// This commit is where HEAD points -> author gets a trailing `*`.
    pub head: bool,
    /// Full names of the branches that contain this commit, shown in the detail
    /// panel's "In N branches" block. A real backend fills this from `git branch
    /// --contains`; the fixtures hardcode a representative set.
    pub containing_branches: Vec<String>,
    /// This is the synthetic "<current>" row representing UNCOMMITTED working-tree
    /// changes (hash == [`WORKING_REV`]), pinned at the top of the log. Its files
    /// pane shows `git status` (working tree vs HEAD); selecting a file opens the
    /// live editable diff vs HEAD. The log column blanks its hash + date.
    pub is_working: bool,
    /// Set ONLY on the synthetic "<current>" row: the uncommitted-changes summary
    /// (branch + per-status counts) the detail pane renders. Carried here so BOTH the
    /// backend's rich detail and the store's cheap [`detail_from`] produce the same
    /// block (the store rebuilds the detail synchronously on every filter change, with
    /// no backend round-trip). `None` for a real commit.
    pub working: Option<WorkingSummary>,
}

/// Sentinel commit hash for the synthetic "<current>" working-tree row. Never a real
/// short hash (non-hex); the backend special-cases it (changed_files -> working tree;
/// open_file/file_view -> base = HEAD blob, right = working tree). The ONE shared
/// constant so the store's selection and the backend's keying agree.
pub const WORKING_REV: &str = "WORKING-TREE";

/// The dynamic "current git user" option in the User filter (first selectable user). It
/// matches commits by [`Commit::is_me`] rather than a literal author name, so it tracks
/// the local git identity even across name spellings.
pub const ME_FILTER: &str = "<me>";

/// Git status of a file within a commit, driving the file-name color in the
/// changed-files pane (Added green, Modified blue, Deleted red). `Unchanged` is
/// only reachable in "All files" mode (the full commit tree): a file the commit
/// did not touch, rendered in the plain default text color (no status accent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    /// Not changed by this commit; shown only in the full-tree ("All") view, in
    /// the plain text color so it does not read as a change.
    Unchanged,
}

/// File-name color by git status: Added green, Modified blue, Deleted red,
/// Unchanged plain text. The single home for the status->color mapping so the
/// files panel and any test assert against one source. PURE (theme + model only).
pub fn status_color(status: FileStatus) -> ratatui::style::Color {
    match status {
        FileStatus::Added => crate::theme::Theme::ACCENT_RUN,
        FileStatus::Modified => crate::theme::Theme::LINK,
        FileStatus::Deleted => crate::theme::Theme::ACCENT_CLOSE,
        FileStatus::Unchanged => crate::theme::Theme::TEXT,
    }
}

/// The foreground color a [`SubjectSpan`] renders in, by its [`SubjectTone`]. The
/// single source the log panel uses so a `<current>` `+N ~N -N` count matches the
/// files-pane status palette exactly.
pub fn subject_color(tone: SubjectTone) -> ratatui::style::Color {
    match tone {
        SubjectTone::Plain => crate::theme::Theme::TEXT,
        SubjectTone::Link => crate::theme::Theme::LINK,
        SubjectTone::Active => crate::theme::Theme::LINK,
        SubjectTone::Dim => crate::theme::Theme::TEXT_DIM,
    }
}

/// A node in the changed-files tree: either a directory (with a file count) or a
/// leaf file. Directories are collapsible.
#[derive(Clone, Debug)]
pub enum TreeNode {
    Dir {
        /// Possibly path-collapsed name, e.g. "packages/transform/go/.../internal".
        name: String,
        file_count: usize,
        expanded: bool,
        children: Vec<TreeNode>,
    },
    File {
        name: String,
        status: FileStatus,
    },
}

/// A flattened, indentation-tagged view of one visible tree row. Produced by
/// walking the tree so the UI can render and select rows by index.
#[derive(Clone, Debug)]
pub struct FlatRow {
    pub depth: usize,
    pub node: FlatKind,
}

#[derive(Clone, Debug)]
pub enum FlatKind {
    Dir {
        name: String,
        file_count: usize,
        expanded: bool,
    },
    File {
        name: String,
        status: FileStatus,
    },
}

/// Full metadata for the commit shown in the bottom detail panel.
#[derive(Clone, Debug)]
pub struct CommitDetail {
    pub subject: String,
    pub short_hash: String,
    pub author: Signature,
    pub committer: Signature,
    /// Full names of the branches that contain this commit (the "In N branches"
    /// block). Empty -> the block is omitted.
    pub containing_branches: Vec<String>,
    /// Set ONLY for the synthetic `<current>` row: the uncommitted-changes summary the
    /// detail pane renders ("Uncommitted changes" + per-status counts + current branch)
    /// instead of the commit subject/hash/author. `None` for a real commit.
    pub working: Option<WorkingSummary>,
}

/// The `<current>` row's uncommitted-changes summary for the detail pane: the current
/// branch (when on one) and the per-status working-tree file counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkingSummary {
    pub branch: Option<String>,
    pub added: usize,
    pub changed: usize,
    pub deleted: usize,
}

/// The repository domain snapshot the UI renders. Backend-agnostic: today it is
/// filled from the fixture backend, later from a real git backend. Holds NO
/// view/selection/focus state - that lives in [`crate::view_state`].
#[derive(Clone, Debug)]
pub struct RepoModel {
    pub commits: Vec<Commit>,
    /// Per-row lane/edge layout for the commit-graph gutter, derived once from the
    /// commit parents by [`crate::graph_engine::build_layout`]. Indexed by row in
    /// `commits` order; the UI renders it, never recomputes it.
    pub graph: GraphLayout,
    /// `None` until the detail for the selected commit has been loaded.
    pub detail: Option<CommitDetail>,
    /// Canonical changed-files tree, including each directory's `expanded` flag.
    pub tree: Vec<TreeNode>,
    /// Full paths in `tree` that match a `.gitignore` rule. Only the All-files view
    /// surfaces ignored (but tracked/force-added) files; the panel renders these
    /// faint. Empty for the changed-only and working trees (those never list ignored
    /// files). Delivered with the tree via [`crate::message::Msg::TreeLoaded`].
    pub ignored: std::collections::HashSet<String>,
    /// Full hashes of commits NOT yet on any remote-tracking branch (what `git push`
    /// would send). The log graph draws these - plus the synthetic `<current>` row - with
    /// a HOLLOW node. Empty when the repo has no remotes (nothing to be ahead of).
    pub unpushed: std::collections::HashSet<String>,
    /// Whether the repo has any configured remote. Distinguishes "no remote so
    /// `unpushed` is empty" from "pushed" when deciding the reword warning (a
    /// remote-less repo never warns "already pushed").
    pub has_remotes: bool,
    /// Diff/source preview for the selected file. `None` -> empty viewer state.
    pub preview: Option<FileView>,
    /// The loaded commits are a CAPPED slice with more history beyond - drives the log's
    /// trailing "Load more history" row. `false` when the whole history is loaded.
    pub more_history: bool,
    /// The (HEAD + working-tree statuses) signature this model was loaded at, from
    /// [`crate::backend::RepoSnapshot::status_sig`]. The periodic status poll compares
    /// against it to detect external changes; `None` until the first real load (a poll
    /// landing before it must not trigger a refresh).
    pub status_sig: Option<u64>,
}

impl RepoModel {
    /// An empty repository (no commits, no tree, empty graph). The honest
    /// no-fixtures constructor: tests build state from this or a hand-built model
    /// without pulling the demo data, and the UI renders its empty state from it.
    pub fn empty() -> Self {
        RepoModel {
            commits: Vec::new(),
            graph: graph_engine::build_layout(&[]),
            detail: None,
            tree: Vec::new(),
            ignored: std::collections::HashSet::new(),
            unpushed: std::collections::HashSet::new(),
            has_remotes: false,
            preview: None,
            more_history: false,
            status_sig: None,
        }
    }
}

impl TreeNode {
    /// Walk the (expanded) tree into a flat list of renderable rows.
    pub fn flatten(nodes: &[TreeNode]) -> Vec<FlatRow> {
        let mut out = Vec::new();
        Self::walk(nodes, 0, &mut out);
        out
    }

    /// Set every directory's `expanded` flag (recursively) to `expanded`. Drives
    /// the files-toolbar Expand All / Collapse All controls. Returns whether any
    /// flag actually changed, so the caller can skip a needless re-clamp/redraw.
    pub fn set_all_expanded(nodes: &mut [TreeNode], expanded: bool) -> bool {
        let mut changed = false;
        for node in nodes.iter_mut() {
            if let TreeNode::Dir {
                expanded: flag,
                children,
                ..
            } = node
            {
                changed |= *flag != expanded;
                *flag = expanded;
                changed |= Self::set_all_expanded(children, expanded);
            }
        }
        changed
    }

    /// Expand every directory that (transitively) contains a CHANGED file (Added /
    /// Modified / Deleted), leaving dirs whose subtree is entirely `Unchanged` as they
    /// are. Returns whether `nodes` holds any changed file, so a parent can expand
    /// itself. Drives the files-toolbar Focus button: in the full-tree view it reveals
    /// every changed file without unfolding noise dirs (the changed-only tree has no
    /// Unchanged rows, so it expands everything there).
    pub fn expand_changed_dirs(nodes: &mut [TreeNode]) -> bool {
        let mut any_changed = false;
        for node in nodes.iter_mut() {
            match node {
                TreeNode::File { status, .. } => any_changed |= *status != FileStatus::Unchanged,
                TreeNode::Dir { expanded, children, .. } => {
                    let child_changed = Self::expand_changed_dirs(children);
                    *expanded |= child_changed;
                    any_changed |= child_changed;
                }
            }
        }
        any_changed
    }

    /// The full, `/`-joined repository path of the FILE at flattened (visible) row
    /// `target`, or `None` when that row is a directory / out of range. Directory
    /// names are accumulated as the prefix so a leaf under a collapsed `"a/b/c"`
    /// node resolves to its real path - the key a real backend's `file_view`
    /// needs (the displayed leaf name alone would not resolve). The fixture path
    /// keys on the leaf suffix, so the trailing component still matches there.
    pub fn path_at(nodes: &[TreeNode], target: usize) -> Option<String> {
        let mut idx = 0;
        let mut prefix = String::new();
        Self::path_walk(nodes, target, &mut idx, &mut prefix)
    }

    fn path_walk(
        nodes: &[TreeNode],
        target: usize,
        idx: &mut usize,
        prefix: &mut String,
    ) -> Option<String> {
        for node in nodes {
            match node {
                TreeNode::Dir { name, expanded, children, .. } => {
                    *idx += 1; // the dir occupies one visible row.
                    if *expanded {
                        let saved = prefix.len();
                        prefix.push_str(name);
                        prefix.push('/');
                        if let Some(path) = Self::path_walk(children, target, idx, prefix) {
                            return Some(path);
                        }
                        prefix.truncate(saved);
                    }
                }
                TreeNode::File { name, .. } => {
                    if *idx == target {
                        return Some(format!("{prefix}{name}"));
                    }
                    *idx += 1;
                }
            }
        }
        None
    }

    /// Full repository paths of every FILE row at or below visible row `target`'s
    /// node: for a FILE row, just its own path; for a DIRECTORY row, every
    /// descendant file's path (recursively, regardless of expand state). Empty when
    /// `target` is out of range. The single source for marking a dir's descendants
    /// (Space/Ctrl-click on a directory) and for resolving a file's mark path.
    pub fn file_paths_under(nodes: &[TreeNode], target: usize) -> Vec<String> {
        let mut idx = 0;
        Self::paths_under_walk(nodes, target, "", &mut idx).unwrap_or_default()
    }

    /// Walk `nodes` tracking the running visible-row `idx`; `Some(paths)` once it
    /// reaches `target` (a file -> itself; a dir -> its whole subtree, ignoring
    /// expand state), `None` while still searching. `base` is the path prefix here.
    /// `Some` vs `None` (not emptiness) signals "found", so a file-less node is safe.
    fn paths_under_walk(
        nodes: &[TreeNode],
        target: usize,
        base: &str,
        idx: &mut usize,
    ) -> Option<Vec<String>> {
        for node in nodes {
            match node {
                TreeNode::Dir { name, expanded, children, .. } => {
                    let child_base = format!("{base}{name}/");
                    if *idx == target {
                        return Some(Self::subtree_files(children, &child_base));
                    }
                    *idx += 1;
                    if *expanded {
                        if let Some(found) = Self::paths_under_walk(children, target, &child_base, idx) {
                            return Some(found);
                        }
                    }
                }
                TreeNode::File { name, .. } => {
                    if *idx == target {
                        return Some(vec![format!("{base}{name}")]);
                    }
                    *idx += 1;
                }
            }
        }
        None
    }

    /// The full repository path (no trailing slash) of the DIRECTORY row at visible row
    /// `target`, or `None` when that row is a file / out of range. The prefix the folder
    /// context menu commits/reverts/patches against (`git ... -- <prefix>`).
    pub fn dir_prefix_at(nodes: &[TreeNode], target: usize) -> Option<String> {
        let mut idx = 0;
        Self::dir_prefix_walk(nodes, target, "", &mut idx)
    }

    fn dir_prefix_walk(nodes: &[TreeNode], target: usize, base: &str, idx: &mut usize) -> Option<String> {
        for node in nodes {
            match node {
                TreeNode::Dir { name, expanded, children, .. } => {
                    if *idx == target {
                        return Some(format!("{base}{name}"));
                    }
                    *idx += 1;
                    let child_base = format!("{base}{name}/");
                    if *expanded {
                        if let Some(found) = Self::dir_prefix_walk(children, target, &child_base, idx) {
                            return Some(found);
                        }
                    }
                }
                TreeNode::File { .. } => {
                    if *idx == target {
                        return None; // a file row, not a directory
                    }
                    *idx += 1;
                }
            }
        }
        None
    }

    /// The changed paths in `nodes` that live under directory `prefix` (the prefix itself
    /// or anything below `prefix/`). The set a folder-scoped Rollback acts on.
    pub fn changed_paths_under(nodes: &[TreeNode], prefix: &str) -> Vec<String> {
        let under = format!("{prefix}/");
        Self::changed_paths(nodes)
            .into_iter()
            .filter(|p| p == prefix || p.starts_with(&under))
            .collect()
    }

    /// Every file path in `nodes` (recursively, ignoring expand state) under the
    /// accumulated `base`. Used to mark all descendants of a directory row.
    fn subtree_files(nodes: &[TreeNode], base: &str) -> Vec<String> {
        let mut out = Vec::new();
        for node in nodes {
            match node {
                TreeNode::Dir { name, children, .. } => {
                    out.extend(Self::subtree_files(children, &format!("{base}{name}/")));
                }
                TreeNode::File { name, .. } => out.push(format!("{base}{name}")),
            }
        }
        out
    }

    /// Every file path in `nodes` whose status is NOT `Unchanged` (i.e. actually
    /// changed in this commit), full `/`-joined. The set of paths a working-tree
    /// revert can meaningfully act on: in the All-files view the tree also carries
    /// Unchanged rows (the full file tree), and reverting one is a no-op that would
    /// still prune the row from the view; filtering against this set excludes them.
    /// The changed-only tree has no Unchanged rows, so this is the identity there.
    pub fn changed_paths(nodes: &[TreeNode]) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        Self::changed_paths_walk(nodes, "", &mut out);
        out
    }

    fn changed_paths_walk(
        nodes: &[TreeNode],
        base: &str,
        out: &mut std::collections::BTreeSet<String>,
    ) {
        for node in nodes {
            match node {
                TreeNode::Dir { name, children, .. } => {
                    Self::changed_paths_walk(children, &format!("{base}{name}/"), out);
                }
                TreeNode::File { name, status } if *status != FileStatus::Unchanged => {
                    out.insert(format!("{base}{name}"));
                }
                TreeNode::File { .. } => {}
            }
        }
    }

    /// The git status of the file at repo-relative `path`, or `None` when no such FILE row
    /// exists in the tree. Drives per-file menu gating - e.g. a HEAD revision exists only for
    /// a non-`Added` (already-tracked) changed file.
    pub fn status_of(nodes: &[TreeNode], path: &str) -> Option<FileStatus> {
        Self::status_of_walk(nodes, "", path)
    }

    fn status_of_walk(nodes: &[TreeNode], base: &str, path: &str) -> Option<FileStatus> {
        for node in nodes {
            match node {
                TreeNode::Dir { name, children, .. } => {
                    let pre = format!("{base}{name}/");
                    if path.starts_with(&pre) {
                        if let Some(s) = Self::status_of_walk(children, &pre, path) {
                            return Some(s);
                        }
                    }
                }
                TreeNode::File { name, status } if format!("{base}{name}") == path => {
                    return Some(*status);
                }
                TreeNode::File { .. } => {}
            }
        }
        None
    }

    /// Build the flattened rows paired with each FILE row's full path (`None` for
    /// directory rows), in one walk. The renderer keys the marked-row affordance on
    /// the path; the index order matches [`Self::flatten`] exactly.
    pub fn flatten_paths(nodes: &[TreeNode]) -> Vec<(FlatRow, Option<String>)> {
        let mut out = Vec::new();
        let mut prefix = String::new();
        Self::flatten_paths_walk(nodes, 0, &mut prefix, &mut out);
        out
    }

    fn flatten_paths_walk(
        nodes: &[TreeNode],
        depth: usize,
        prefix: &mut String,
        out: &mut Vec<(FlatRow, Option<String>)>,
    ) {
        for node in nodes {
            match node {
                TreeNode::Dir { name, file_count, expanded, children } => {
                    out.push((
                        FlatRow {
                            depth,
                            node: FlatKind::Dir {
                                name: name.clone(),
                                file_count: *file_count,
                                expanded: *expanded,
                            },
                        },
                        None,
                    ));
                    if *expanded {
                        let saved = prefix.len();
                        prefix.push_str(name);
                        prefix.push('/');
                        Self::flatten_paths_walk(children, depth + 1, prefix, out);
                        prefix.truncate(saved);
                    }
                }
                TreeNode::File { name, status } => out.push((
                    FlatRow {
                        depth,
                        node: FlatKind::File { name: name.clone(), status: *status },
                    },
                    Some(format!("{prefix}{name}")),
                )),
            }
        }
    }

    /// A FLAT view of the changed files: one `File` row per file (depth 0), its
    /// FULL `/`-joined path as the displayed name, with NO directory rows. Drives
    /// the files-toolbar "Flat" toggle (changed files without folders). Pairs each
    /// row with its path so the index->path lookups match what is rendered. The
    /// walk descends every directory regardless of its `expanded` flag (flat shows
    /// all files).
    pub fn flatten_flat(nodes: &[TreeNode]) -> Vec<(FlatRow, Option<String>)> {
        let mut out = Vec::new();
        Self::flatten_flat_walk(nodes, &mut String::new(), &mut out);
        out
    }

    fn flatten_flat_walk(
        nodes: &[TreeNode],
        prefix: &mut String,
        out: &mut Vec<(FlatRow, Option<String>)>,
    ) {
        for node in nodes {
            match node {
                TreeNode::Dir { name, children, .. } => {
                    let saved = prefix.len();
                    prefix.push_str(name);
                    prefix.push('/');
                    Self::flatten_flat_walk(children, prefix, out);
                    prefix.truncate(saved);
                }
                TreeNode::File { name, status } => {
                    let path = format!("{prefix}{name}");
                    out.push((
                        FlatRow {
                            depth: 0,
                            node: FlatKind::File { name: path.clone(), status: *status },
                        },
                        Some(path),
                    ));
                }
            }
        }
    }

    /// The visible files rows honoring the Flat toggle: the nested tree flatten when
    /// `flat` is false, else the folder-free flat list. Each row paired with its
    /// file path. The ONE flat-aware producer the store and files panel both read,
    /// so their row indices always agree with what is rendered.
    pub fn flatten_paths_view(nodes: &[TreeNode], flat: bool) -> Vec<(FlatRow, Option<String>)> {
        if flat {
            Self::flatten_flat(nodes)
        } else {
            Self::flatten_paths(nodes)
        }
    }

    /// Remove every FILE node whose full `/`-joined path is in `paths`, then drop any
    /// directory left with no children and refresh each surviving directory's
    /// `file_count`. PURE in-memory tree mutation (ZERO IO): the git write already
    /// happened in the loader; this only updates the displayed tree so reverted files
    /// disappear from the pane. Reverting every file empties `nodes` (the empty state
    /// renders without panic). The single prune helper used by `store::apply`.
    pub fn prune_paths(nodes: &mut Vec<TreeNode>, paths: &[String]) {
        let removed: std::collections::BTreeSet<&str> = paths.iter().map(String::as_str).collect();
        Self::prune_walk(nodes, "", &removed);
    }

    /// Recursive worker for [`Self::prune_paths`]: drop matching files under the
    /// running `base` prefix, recurse into directories, then drop emptied dirs and
    /// recompute each kept dir's `file_count` from its surviving descendants.
    fn prune_walk(
        nodes: &mut Vec<TreeNode>,
        base: &str,
        removed: &std::collections::BTreeSet<&str>,
    ) {
        nodes.retain_mut(|node| match node {
            TreeNode::File { name, .. } => !removed.contains(format!("{base}{name}").as_str()),
            TreeNode::Dir {
                name,
                file_count,
                children,
                ..
            } => {
                let child_base = format!("{base}{name}/");
                Self::prune_walk(children, &child_base, removed);
                *file_count = Self::subtree_files(children, "").len();
                !children.is_empty()
            }
        });
    }

    /// Toggle the `expanded` flag of the directory shown at visible row
    /// `target`. Returns `true` if a directory was toggled (files do nothing).
    pub fn toggle_visible(nodes: &mut [TreeNode], target: usize) -> bool {
        let mut idx = 0;
        Self::toggle_walk(nodes, target, &mut idx)
    }

    fn toggle_walk(nodes: &mut [TreeNode], target: usize, idx: &mut usize) -> bool {
        for node in nodes.iter_mut() {
            match node {
                TreeNode::Dir {
                    expanded, children, ..
                } => {
                    if *idx == target {
                        *expanded = !*expanded;
                        return true;
                    }
                    *idx += 1;
                    if *expanded && Self::toggle_walk(children, target, idx) {
                        return true;
                    }
                }
                TreeNode::File { .. } => {
                    if *idx == target {
                        return false;
                    }
                    *idx += 1;
                }
            }
        }
        false
    }

    fn walk(nodes: &[TreeNode], depth: usize, out: &mut Vec<FlatRow>) {
        for node in nodes {
            match node {
                TreeNode::Dir {
                    name,
                    file_count,
                    expanded,
                    children,
                } => {
                    out.push(FlatRow {
                        depth,
                        node: FlatKind::Dir {
                            name: name.clone(),
                            file_count: *file_count,
                            expanded: *expanded,
                        },
                    });
                    if *expanded {
                        Self::walk(children, depth + 1, out);
                    }
                }
                TreeNode::File { name, status } => out.push(FlatRow {
                    depth,
                    node: FlatKind::File {
                        name: name.clone(),
                        status: *status,
                    },
                }),
            }
        }
    }
}

// -- search + filter: the single filtered view of the log -------------------
//
// All pure (model-only deps). `visible_commits` is the one source of which
// commits the log panel renders and which the selection indexes into; nothing
// duplicates `Commit`. `apply` stays ZERO IO: the regex is compiled here, and an
// invalid pattern yields `None` (no matches) rather than panicking.

/// The four fixed `Date` filter presets, in dropdown order. `Last *` bucket the
/// commit dates relative to the newest commit (a deterministic "now" for static
/// fixtures); `[0]` is the "All" reset that maps to no filter.
const DATE_PRESETS: [&str; 4] = ["All", "Last 24 hours", "Last 7 days", "Last 30 days"];

/// Indices into `repo.commits` for the commits passing the search query AND every
/// active filter, in original order. Empty query + no filter -> all commits. The
/// single source of the filtered log view, shared by the panel and selection.
pub fn visible_commits(repo: &RepoModel, view: &ViewState) -> Vec<usize> {
    let re = compile_query(view);
    (0..repo.commits.len())
        .filter(|&i| {
            let c = &repo.commits[i];
            // The synthetic "<current>" row is always pinned at the top - filters and
            // search target real commits, never the uncommitted-changes summary.
            c.is_working
                || (query_match(c, view, re.as_ref())
                    && filter_match(repo, c, FilterKind::User, view)
                    && filter_match(repo, c, FilterKind::Branch, view)
                    && filter_match(repo, c, FilterKind::Date, view))
        })
        .collect()
}

/// The commit currently selected in the log: maps `view.log_sel` (an index into the
/// FILTERED list) back to a `repo.commits` entry. `None` over an empty / out-of-range
/// selection. The single source the files-pane "A vs B" header reads.
pub fn selected_commit<'a>(repo: &'a RepoModel, view: &ViewState) -> Option<&'a Commit> {
    visible_commits(repo, view)
        .get(view.log_sel)
        .and_then(|&i| repo.commits.get(i))
}

/// The visible files-pane rows for the panel: the changed-files tree directly (no synthetic
/// root row). The ONE source the store's index lookups, the row count, and the files panel
/// render all read, so a visible row index maps 1:1 to a tree row - no offset. The rows come
/// from [`inner_file_rows`] (flat-aware + search-narrowed); see that for the matching rules.
pub fn visible_file_rows(repo: &RepoModel, view: &ViewState) -> Vec<(FlatRow, Option<String>)> {
    inner_file_rows(repo, view)
}

/// The changed-files tree rows (NO synthetic root), honoring the Flat toggle and the
/// active files-search. With no query this is the flat-aware flatten; with a query the
/// results are ALWAYS presented FLAT (full path per row, no folders) and narrowed to the
/// files matching the query by PATH (a flat flatten descends collapsed dirs, so a match
/// buried inside a folded folder still surfaces). Matching is always case-insensitive;
/// `files_regex_on` treats the query as a regex (invalid -> no match).
fn inner_file_rows(repo: &RepoModel, view: &ViewState) -> Vec<(FlatRow, Option<String>)> {
    if view.files_search.is_empty() {
        return TreeNode::flatten_paths_view(&repo.tree, view.files_flat);
    }
    let re = view
        .files_regex_on
        .then(|| Regex::new(&format!("(?i){}", view.files_search)).ok())
        .flatten();
    let needle = view.files_search.to_lowercase();
    let matches = |path: &str| -> bool {
        if view.files_regex_on {
            re.as_ref().is_some_and(|r| r.is_match(path).unwrap_or(false))
        } else {
            path.to_lowercase().contains(&needle)
        }
    };
    TreeNode::flatten_flat(&repo.tree)
        .into_iter()
        .filter(|(_, path)| path.as_deref().is_some_and(&matches))
        .collect()
}

/// Compile the search query into a regex when the `.*` toggle is on. Returns
/// `None` when regex mode is off (the substring path is used) OR when the pattern
/// fails to compile (invalid pattern -> no matches, never a panic). Matching is
/// always case-insensitive (an inline `(?i)` flag; there is no case toggle).
pub fn compile_query(view: &ViewState) -> Option<Regex> {
    if !view.regex_on || view.search.is_empty() {
        return None;
    }
    Regex::new(&format!("(?i){}", view.search)).ok()
}

/// Whether commit `c` matches the search query. The query is tested against the
/// subject, author, BOTH the short and full hash (so a partial-hash search hits),
/// and every ref label on the row (tag / branch names). Empty query -> always true.
/// In regex mode `re` must be `Some` (a `None` there means the pattern was invalid
/// -> no matches).
fn query_match(c: &Commit, view: &ViewState, re: Option<&Regex>) -> bool {
    if view.search.is_empty() {
        return true;
    }
    let subject = commit_subject(c);
    let mut fields = vec![
        subject.as_str(),
        c.author.as_str(),
        c.hash.as_str(),
        c.full_hash.as_str(),
    ];
    fields.extend(c.refs.iter().map(|r| r.name.as_str()));
    if view.regex_on {
        match re {
            Some(re) => fields.iter().any(|h| re.is_match(h).unwrap_or(false)),
            None => false,
        }
    } else {
        // Plain substring match, case-insensitive (no case toggle).
        let needle = view.search.to_lowercase();
        fields.iter().any(|h| h.to_lowercase().contains(&needle))
    }
}

/// Join a commit's subject spans into a single searchable/labelable string.
pub fn commit_subject(c: &Commit) -> String {
    c.subject.iter().map(|s| s.text.as_str()).collect()
}

/// Whether commit `c` passes filter `kind` given the view's selection. `None`
/// selection ("All") always passes; otherwise the per-kind check runs against the
/// selected option (`Date` consults the repo for its anchor).
fn filter_match(repo: &RepoModel, c: &Commit, kind: FilterKind, view: &ViewState) -> bool {
    let sel = match view.filter(kind) {
        Some(s) => s,
        None => return true,
    };
    match kind {
        // The dynamic "<me>" option matches the LOCAL git identity's commits (`is_me`,
        // stamped from `current_user`); any other option matches the literal author.
        FilterKind::User if sel == ME_FILTER => c.is_me,
        FilterKind::User => c.author == sel,
        // Branch membership is the FULL set of branches that contain this commit
        // (filled by the backend via `git branch --contains`), NOT the tip-ref
        // decoration: picking a branch must select its whole reachable history,
        // not only the single commit its ref happens to decorate.
        FilterKind::Branch => c.containing_branches.iter().any(|b| b == sel),
        FilterKind::Date => date_within(repo, c, sel),
    }
}

/// Whether `c`'s date falls within the selected `Last *` preset. The "now" anchor
/// is the newest commit date (deterministic for static fixtures). Unparseable
/// dates or unknown presets -> false.
fn date_within(repo: &RepoModel, c: &Commit, preset: &str) -> bool {
    let window = match preset {
        "Last 24 hours" => 1,
        "Last 7 days" => 7,
        "Last 30 days" => 30,
        _ => return false,
    };
    let commit_day = match parse_date(&c.date) {
        Some(d) => d,
        None => return false,
    };
    let newest = repo
        .commits
        .iter()
        .filter_map(|c| parse_date(&c.date))
        .max();
    match newest {
        Some(n) => n.saturating_sub(commit_day) <= window,
        None => false,
    }
}

/// Parse either configured commit-date shape into a day ordinal (only ordering and
/// day-differences matter, so a proleptic `year*372 + month*31 + day` count is
/// enough). Handles BOTH `[behavior].date_format` values so the Date filter works
/// regardless of the rendered format: `"DD.MM.YYYY, HH:MM"` (Dmy) and
/// `"YYYY-MM-DD HH:MM"` (Iso). `None` on any malformed field.
fn parse_date(s: &str) -> Option<i64> {
    // Drop the time: Dmy separates it with ", ", Iso with a bare space.
    let date = s.split([',', ' ']).next()?.trim();
    let (year, month, day) = if date.contains('-') {
        // ISO: YYYY-MM-DD.
        let mut parts = date.split('-');
        let year: i64 = parts.next()?.trim().parse().ok()?;
        let month: i64 = parts.next()?.trim().parse().ok()?;
        let day: i64 = parts.next()?.trim().parse().ok()?;
        (year, month, day)
    } else {
        // Dmy: DD.MM.YYYY.
        let mut parts = date.split('.');
        let day: i64 = parts.next()?.trim().parse().ok()?;
        let month: i64 = parts.next()?.trim().parse().ok()?;
        let year: i64 = parts.next()?.trim().parse().ok()?;
        (year, month, day)
    };
    Some(year * 372 + month * 31 + day)
}

/// Distinct, sorted options for filter `kind`, prefixed with the "All" reset row
/// at index 0. `User`/`Branch` derive from the fixtures; `Date` is fixed
/// presets. Pure: reads only the repo.
pub fn filter_options(repo: &RepoModel, kind: FilterKind) -> Vec<String> {
    match kind {
        FilterKind::Date => DATE_PRESETS.iter().map(|s| s.to_string()).collect(),
        // "All", then the dynamic "<me>" (the local git identity), then the distinct
        // real authors. The synthetic "<current>" row's author is excluded so it never
        // pollutes the list (its author is surfaced as "<me>" instead).
        FilterKind::User => {
            let mut opts = with_all(distinct_sorted(
                repo.commits.iter().filter(|c| !c.is_working).map(|c| c.author.clone()),
            ));
            opts.insert(1, ME_FILTER.to_string());
            opts
        }
        // Every branch that contains ANY loaded commit, so a branch whose tip is
        // buried outside the loaded window still appears (its history overlaps the
        // window). Derived from `containing_branches`, not the tip-ref decoration.
        FilterKind::Branch => with_all(distinct_sorted(
            repo.commits.iter().flat_map(|c| c.containing_branches.iter().cloned()),
        )),
    }
}

/// Prepend the "All" reset row (row 0 -> clear the filter) to an option list.
fn with_all(mut opts: Vec<String>) -> Vec<String> {
    opts.insert(0, "All".to_string());
    opts
}

/// Collect, sort, and dedup an iterator of strings.
fn distinct_sorted(it: impl Iterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = it.collect();
    v.sort();
    v.dedup();
    v
}

/// The toolbar label + active flag for filter `kind`: `"User: <sel>"` (active)
/// when a selection is set, else the bare kind name (inactive/"All").
pub fn filter_label(kind: FilterKind, view: &ViewState) -> (String, bool) {
    let name = filter_name(kind);
    match view.filter(kind) {
        Some(sel) => (format!("{name}: {sel}"), true),
        None => (name.to_string(), false),
    }
}

/// The bare display name of a filter kind.
pub fn filter_name(kind: FilterKind) -> &'static str {
    match kind {
        FilterKind::Branch => "Branch",
        FilterKind::User => "User",
        FilterKind::Date => "Date",
    }
}

/// Whether the detail panel's "committed by" line shows: only when the committer
/// differs from the author. The detail renderer reads it so the signature block and
/// the "In N branches" header row land in the right place.
pub fn detail_has_committer_line(detail: &CommitDetail) -> bool {
    detail.committer.name != detail.author.name || detail.committer.email != detail.author.email
}

/// Derive a [`CommitDetail`] for the bottom panel from a commit row, so the
/// detail follows the (filtered) selection (the cheap ZERO-IO placeholder built
/// in `apply`; a later `Msg::DetailLoaded` upgrades it with the real committer).
/// A commit row carries no committer, so the author doubles as committer here.
pub fn detail_from(c: &Commit) -> CommitDetail {
    let author = Signature {
        name: c.author.clone(),
        email: String::new(),
        when: c.date.clone(),
    };
    CommitDetail {
        subject: commit_subject(c),
        // The synthetic working row has no commit hash to show in the detail chip.
        short_hash: if c.is_working { String::new() } else { c.hash.clone() },
        committer: author.clone(),
        author,
        containing_branches: c.containing_branches.clone(),
        // The working row carries its summary on the row itself, so both this cheap
        // path and the backend's rich detail render the same block.
        working: c.working.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view_state::ViewState;

    /// A commit row with a date + branch membership, the two fields the filter fixes
    /// read. Subject/author are fixed; refs stay empty (the Branch filter no longer
    /// uses them).
    fn commit(hash: &str, date: &str, branches: &[&str]) -> Commit {
        Commit {
            hash: hash.to_string(),
            full_hash: hash.to_string(),
            parents: vec![],
            subject: vec![SubjectSpan::plain("s")],
            refs: vec![],
            author: "A".to_string(),
            date: date.to_string(),
            date_label: date.to_string(),
            is_me: false,
            head: false,
            containing_branches: branches.iter().map(|s| s.to_string()).collect(),
            is_working: false,
            working: None,
        }
    }

    fn repo_of(commits: Vec<Commit>) -> RepoModel {
        let mut repo = RepoModel::empty();
        repo.graph = graph_engine::build_layout(&commits);
        repo.commits = commits;
        repo
    }

    #[test]
    fn status_color_maps_unchanged_to_plain_text() {
        use crate::theme::Theme;
        // Each status maps to its accent; Unchanged is the plain default text color
        // (no green/red/blue) so an untouched file in the All view reads as plain.
        assert_eq!(status_color(FileStatus::Added), Theme::ACCENT_RUN);
        assert_eq!(status_color(FileStatus::Modified), Theme::LINK);
        assert_eq!(status_color(FileStatus::Deleted), Theme::ACCENT_CLOSE);
        assert_eq!(status_color(FileStatus::Unchanged), Theme::TEXT);
    }

    /// `visible_file_rows` with a query presents matches FLAT (full path per row, surfacing
    /// files inside collapsed dirs), is case-insensitive, and honors the `.*` regex toggle.
    /// With no query it is the normal tree flatten.
    #[test]
    fn files_search_filters_flat_by_path() {
        let mut repo = RepoModel::empty();
        repo.tree = vec![
            TreeNode::Dir {
                name: "src".to_string(),
                file_count: 2,
                expanded: true,
                children: vec![
                    TreeNode::File { name: "main.rs".to_string(), status: FileStatus::Modified },
                    TreeNode::File { name: "store.rs".to_string(), status: FileStatus::Modified },
                ],
            },
            TreeNode::File { name: "README.md".to_string(), status: FileStatus::Modified },
        ];
        let names = |repo: &RepoModel, view: &ViewState| -> Vec<String> {
            visible_file_rows(repo, view)
                .iter()
                .map(|(r, _)| match &r.node {
                    FlatKind::Dir { name, .. } => name.clone(),
                    FlatKind::File { name, .. } => name.clone(),
                })
                .collect()
        };
        let mut view = ViewState::new(0);
        // No query -> the normal tree flatten (dir + its two files + the root file).
        assert_eq!(names(&repo, &view), vec!["src", "main.rs", "store.rs", "README.md"]);
        // Substring "store" -> the matching file by FULL PATH (presented flat).
        view.files_search = "store".to_string();
        assert_eq!(names(&repo, &view), vec!["src/store.rs"]);
        // Case-insensitive.
        view.files_search = "STORE".to_string();
        assert_eq!(names(&repo, &view), vec!["src/store.rs"]);
        // Regex "ma.n" matches src/main.rs.
        view.files_regex_on = true;
        view.files_search = "ma.n".to_string();
        assert_eq!(names(&repo, &view), vec!["src/main.rs"]);
        // A root-level file matches by its bare name.
        view.files_regex_on = false;
        view.files_search = "readme".to_string();
        assert_eq!(names(&repo, &view), vec!["README.md"]);
        // No match -> the pane is empty (the tree narrows to nothing).
        view.files_search = "zzz".to_string();
        assert!(names(&repo, &view).is_empty());
        assert!(visible_file_rows(&repo, &view).is_empty(), "an unmatched query leaves no rows");
    }

    #[test]
    fn parse_date_handles_dmy_and_iso() {
        // Same instant, both configured shapes -> the same day ordinal.
        let dmy = parse_date("22.05.2026, 12:08").unwrap();
        let iso = parse_date("2026-05-22 12:08").unwrap();
        assert_eq!(dmy, iso, "DMY and ISO parse to the same day ordinal");
        // A day-later ISO date is one larger; malformed input is None.
        assert_eq!(parse_date("2026-05-23 00:00").unwrap(), iso + 1);
        assert!(parse_date("not a date").is_none());
    }

    #[test]
    fn date_filter_buckets_under_both_formats() {
        // Newest at day +30; an older commit at day 0 is outside "Last 7 days".
        for (new, old) in [
            ("20.06.2026, 00:00", "21.05.2026, 00:00"),
            ("2026-06-20 00:00", "2026-05-21 00:00"),
        ] {
            let repo = repo_of(vec![commit("h0", new, &[]), commit("h1", old, &[])]);
            let mut view = ViewState::new(0);
            *view.filter_mut(FilterKind::Branch) = None;
            *view.filter_mut(FilterKind::Date) = Some("Last 7 days".to_string());
            assert_eq!(visible_commits(&repo, &view), vec![0], "only the newest is within 7 days");
        }
    }

    #[test]
    fn prune_paths_removes_files_and_empty_dirs() {
        // A dir with two files plus a sibling file. Reverting both dir files removes
        // them AND the now-empty dir; the sibling stays.
        let mut tree = vec![
            TreeNode::Dir {
                name: "src".to_string(),
                file_count: 2,
                expanded: true,
                children: vec![
                    TreeNode::File { name: "a.go".to_string(), status: FileStatus::Modified },
                    TreeNode::File { name: "b.go".to_string(), status: FileStatus::Added },
                ],
            },
            TreeNode::File { name: "root.go".to_string(), status: FileStatus::Deleted },
        ];
        TreeNode::prune_paths(&mut tree, &["src/a.go".to_string(), "src/b.go".to_string()]);
        let rows = TreeNode::flatten(&tree);
        assert_eq!(rows.len(), 1, "the emptied dir and its files are gone");
        assert!(
            matches!(&rows[0].node, FlatKind::File { name, .. } if name == "root.go"),
            "only the sibling file survives"
        );

        // Removing one of a dir's two files keeps the dir and refreshes its count.
        let mut tree = vec![TreeNode::Dir {
            name: "src".to_string(),
            file_count: 2,
            expanded: true,
            children: vec![
                TreeNode::File { name: "a.go".to_string(), status: FileStatus::Modified },
                TreeNode::File { name: "b.go".to_string(), status: FileStatus::Added },
            ],
        }];
        TreeNode::prune_paths(&mut tree, &["src/a.go".to_string()]);
        match &tree[0] {
            TreeNode::Dir { file_count, children, .. } => {
                assert_eq!(*file_count, 1, "file_count refreshed to the survivor count");
                assert_eq!(children.len(), 1, "only b.go remains under src");
            }
            _ => panic!("the dir must survive a partial prune"),
        }

        // Pruning every path empties the tree (no panic).
        let mut tree = vec![TreeNode::File { name: "x.go".to_string(), status: FileStatus::Added }];
        TreeNode::prune_paths(&mut tree, &["x.go".to_string()]);
        assert!(tree.is_empty(), "reverting the last file empties the tree");
    }

    #[test]
    fn branch_filter_uses_containing_branches_not_tip_refs() {
        // Two commits on `main`; only the tip also on `feat`. Picking `main` selects
        // BOTH (full history), not just the tip - the by-containment fix.
        let repo = repo_of(vec![
            commit("tip", "01.01.2026, 00:00", &["main", "feat"]),
            commit("base", "01.01.2026, 00:00", &["main"]),
        ]);
        let mut view = ViewState::new(0);
        *view.filter_mut(FilterKind::Branch) = Some("main".to_string());
        assert_eq!(visible_commits(&repo, &view), vec![0, 1], "main selects its whole history");
        *view.filter_mut(FilterKind::Branch) = Some("feat".to_string());
        assert_eq!(visible_commits(&repo, &view), vec![0], "feat selects only the tip");

        // Options list every branch reaching any loaded commit (sorted, All-prefixed).
        let opts = filter_options(&repo, FilterKind::Branch);
        assert_eq!(opts, vec!["All", "feat", "main"], "branch options from containment");
    }

    #[test]
    fn expand_changed_dirs_unfolds_only_dirs_with_a_change() {
        let dir = |name: &str, children: Vec<TreeNode>| TreeNode::Dir {
            name: name.to_string(),
            file_count: 0,
            expanded: false,
            children,
        };
        let file = |name: &str, status: FileStatus| TreeNode::File { name: name.to_string(), status };
        // `changed/` holds a Modified file (must unfold); `clean/` is all Unchanged (stays
        // folded); a nested `clean/deep/` with a change must unfold both levels.
        let mut tree = vec![
            dir("changed", vec![file("a.go", FileStatus::Modified)]),
            dir("clean", vec![file("b.go", FileStatus::Unchanged)]),
            dir("mixed", vec![dir("deep", vec![file("c.go", FileStatus::Added)])]),
        ];
        let any = TreeNode::expand_changed_dirs(&mut tree);
        assert!(any, "the tree holds changed files");
        let expanded = |n: &TreeNode| matches!(n, TreeNode::Dir { expanded, .. } if *expanded);
        assert!(expanded(&tree[0]), "changed/ unfolds");
        assert!(!expanded(&tree[1]), "clean/ stays folded");
        assert!(expanded(&tree[2]), "mixed/ unfolds (its subtree has a change)");
        if let TreeNode::Dir { children, .. } = &tree[2] {
            assert!(expanded(&children[0]), "mixed/deep/ unfolds too");
        }
    }
}
