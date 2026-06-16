//! End-to-end feature matrix over a REAL tempdir git repository.
//!
//! Every test builds an on-disk repo with `git2` (an initial commit, a second
//! commit that ADDS + MODIFIES + DELETES files, a feature branch, and a MERGE
//! commit), opens it through the crate's public `open_real_backend` /
//! `bootstrap_real` composition roots, and drives each interactive feature through
//! the lib API exactly as the runtime does - applying `Msg`s to an `AppState` and,
//! where a feature is render-dependent, rendering `ui::view` into a ratatui
//! `TestBackend` buffer and asserting on cells. This enforces the audited feature
//! matrix on REAL git data (not just fixtures), so a regression breaks `cargo test`.
//!
//! It reuses the tempdir + `TestBackend` patterns from `tests/loader.rs` and the
//! `src/snapshot.rs` render tests. Revert is now REAL (working-tree-only, modal
//! confirmation-gated): the apply-side modal flow and the backend `revert_file` are
//! asserted here on the tempdir repo, never on the project repo.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use git2::{Commit, IndexAddOption, Oid, Repository, Signature, Time};

use gitgit::config::Config;
use gitgit::diff::{FileView, LineKind};
use gitgit::message::Msg;
use gitgit::model::{visible_commits, visible_file_rows, FileStatus, FlatKind, RepoModel, Status, TreeNode};
use gitgit::store::AppState;
use gitgit::theme::Theme;
use gitgit::view_state::{DiffMode, FilterKind, MenuId, Pane, ViewState};

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::Terminal;

// -- tempdir repo harness ----------------------------------------------------

/// A unique temp dir, removed on drop, so each test repo stays isolated.
struct TempRepo {
    dir: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("gitgit-features-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempRepo { dir }
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Write `contents` to `name` under the repo dir, creating parent dirs.
fn write(root: &Path, name: &str, contents: &str) {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// A signature at a fixed epoch + `offset_days` past it, so the Date filter has
/// real, ordered commit dates. 2026-05-22 12:08 UTC is the epoch anchor.
fn sig_at(offset_days: i64) -> Signature<'static> {
    let epoch = 1_779_451_680 + offset_days * 86_400;
    Signature::new("Alice Dev", "alice@example.com", &Time::new(epoch, 0)).unwrap()
}

/// Stage the whole work tree and commit it (author == committer unless `committer`
/// is given) on top of `parents`, moving `HEAD`. Returns the new oid.
fn commit(
    repo: &Repository,
    message: &str,
    parents: &[Oid],
    author: &Signature,
    committer: &Signature,
) -> Oid {
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let parent_commits: Vec<Commit> = parents.iter().map(|p| repo.find_commit(*p).unwrap()).collect();
    let parent_refs: Vec<&Commit> = parent_commits.iter().collect();
    repo.commit(Some("HEAD"), author, committer, message, &tree, &parent_refs)
        .unwrap()
}

/// A repo with a non-trivial real history:
///   c1 (root, day 0): adds a.go, keep.txt, sub/gone.go
///   c2 (day 3):       modifies a.go, ADDS b.go, DELETES sub/gone.go
///   feature branch off c2:
///     f1 (day 4):     adds feature.go (author Bob)
///   merge (day 6):    c2 + f1 -> a real 2-parent merge commit on the default branch
/// plus an extra branch `release` whose tip is c2 (a buried, non-HEAD tip).
struct Built {
    tmp: TempRepo,
    backend: std::sync::Arc<dyn gitgit::backend::GitBackend + Send + Sync>,
    default_branch: String,
    c1: String,
    c2: String,
    f1: String,
    merge: String,
}

fn build_repo() -> Built {
    let tmp = TempRepo::new();
    let repo = Repository::init(&tmp.dir).unwrap();

    // c1: root.
    write(&tmp.dir, "a.go", "package main\n\nfunc A() {}\n");
    write(&tmp.dir, "keep.txt", "unchanged\n");
    write(&tmp.dir, "sub/gone.go", "package sub\n");
    let s0 = sig_at(0);
    let c1 = commit(&repo, "root: initial files", &[], &s0, &s0);

    // c2: modify a.go, add b.go, delete sub/gone.go.
    write(&tmp.dir, "a.go", "package main\n\nfunc A() { return }\n");
    write(&tmp.dir, "b.go", "package main\n\nfunc B() {}\n");
    std::fs::remove_file(tmp.dir.join("sub/gone.go")).unwrap();
    // Dates are spaced so the Date-filter presets bucket cleanly relative to the
    // newest commit (the merge, day 30): root(day 0) falls OUTSIDE 7 days, delta
    // (day 24) and feat (day 25) inside it; only the merge is within 24h.
    let s24 = sig_at(24);
    let c2 = commit(&repo, "delta: change a, add b, drop gone", &[c1], &s24, &s24);

    // Capture the original default branch (master/main) BEFORE we switch to feature.
    let default_branch = default_branch_name(&repo);

    // A release branch pinned at c2 (a buried, non-HEAD tip).
    repo.branch("release", &repo.find_commit(c2).unwrap(), false).unwrap();

    // feature branch off c2 -> f1 (authored by Bob, committed by Alice).
    repo.branch("feature", &repo.find_commit(c2).unwrap(), false).unwrap();
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    write(&tmp.dir, "feature.go", "package main\n\nfunc Feature() {}\n");
    let bob = Signature::new("Bob Maint", "bob@example.com", &Time::new(1_779_451_680 + 25 * 86_400, 0)).unwrap();
    let alice25 = sig_at(25);
    let f1 = commit(&repo, "feat: add feature.go", &[c2], &bob, &alice25);

    // Back to the default branch and merge feature in (a real 2-parent commit).
    repo.set_head(&format!("refs/heads/{default_branch}")).unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    let s30 = sig_at(30);
    let merge = commit(&repo, "Merge branch 'feature'", &[c2, f1], &s30, &s30);

    let backend = gitgit::open_real_backend(&tmp.dir, &Config::default()).unwrap();
    Built {
        tmp,
        backend,
        default_branch,
        c1: short(c1),
        c2: short(c2),
        f1: short(f1),
        merge: short(merge),
    }
}

/// The default branch name `git init` produced (master or main).
fn default_branch_name(repo: &Repository) -> String {
    let head = repo.head().unwrap();
    head.shorthand().unwrap().to_string()
}

/// Short hash matching the backend's `SHORT_HASH_LEN` (8) so test expectations line
/// up with the rendered/loaded commit hashes.
fn short(oid: Oid) -> String {
    oid.to_string().chars().take(8).collect()
}

/// Build a fully-resolved `AppState` for the tempdir repo via the public synchronous
/// composition root (loads repo + detail + tree + preview for the default selection).
fn state_for(built: &Built) -> AppState {
    gitgit::bootstrap_real(&built.tmp.dir, &Config::default()).unwrap()
}

/// A state selected on commit `c2` (the rich add/modify/delete tree), with the
/// Files pane focused and the cursor on the first FILE row. The merge HEAD has an
/// empty diff vs its mainline parent here, so the revert tests drive c2 instead.
/// Loads c2's tree exactly as the runtime's `Msg::TreeLoaded` would.
fn state_on_c2(built: &Built) -> AppState {
    let mut state = state_for(built);
    // Select c2's log row (newest-first: merge=0, f1=1, c2=2, root=3).
    let row = visible_commits(&state.repo, &state.view)
        .iter()
        .position(|&i| state.repo.commits[i].hash == built.c2)
        .expect("c2 is in the visible log");
    state.apply(Msg::SelectCommit(row));
    let tree = built.backend.changed_files(&built.c2).unwrap();
    state.apply(Msg::TreeLoaded { hash: built.c2.clone(), tree, ignored: Default::default() });
    state.view.focus = Pane::Files;
    // Park the cursor on the first FILE row (VISIBLE index) so a single-target revert has a file.
    let file_row = visible_file_rows(&state.repo, &state.view)
        .iter()
        .position(|(r, _)| matches!(r.node, FlatKind::File { .. }))
        .expect("c2's tree has a file row");
    state.apply(Msg::SelectFile(file_row));
    state
}

// -- render helpers (TestBackend) -------------------------------------------

fn render(state: &AppState, w: u16, h: u16) -> Buffer {
    render_status(state, w, h, &Status::Ready)
}

fn render_status(state: &AppState, w: u16, h: u16, status: &Status) -> Buffer {
    render_parts(&state.repo, &state.view, status, w, h)
}

fn render_parts(repo: &RepoModel, view: &ViewState, status: &Status, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|frame| gitgit::ui::view(frame, repo, view, status))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Row `y` flattened to a string for substring asserts.
fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).map_or("", |c| c.symbol()))
        .collect()
}

/// The whole buffer flattened to one searchable string.
fn whole_text(buf: &Buffer) -> String {
    (0..buf.area.height).map(|y| row_text(buf, y)).collect()
}

/// Whether any cell anywhere carries background `bg`.
fn any_bg(buf: &Buffer, bg: Color) -> bool {
    (0..buf.area.height).any(|y| (0..buf.area.width).any(|x| buf.cell((x, y)).map(|c| c.bg) == Some(bg)))
}

/// The fg color of the FIRST cell of the first occurrence of `needle` on any row,
/// or `None` if the text is not present. Matches the needle against CONSECUTIVE
/// cell symbols (column-indexed), NOT a byte offset into the flattened row, so it
/// stays correct on rows that carry multi-byte glyphs (the graph gutter, ref chips).
fn fg_of_text(buf: &Buffer, needle: &str) -> Option<Color> {
    let chars: Vec<char> = needle.chars().collect();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if (0..chars.len() as u16).all(|d| {
                buf.cell((x + d, y))
                    .map(|c| c.symbol()) == Some(chars[d as usize].to_string().as_str())
            }) {
                return buf.cell((x, y)).map(|c| c.fg);
            }
        }
    }
    None
}

// -- CLUSTER A: commit log + parents/graph ----------------------------------

#[test]
fn commit_list_order_parents_and_real_merge_graph() {
    let built = build_repo();
    let snap = built.backend.load_repo().unwrap();
    let model = gitgit::backend::build_repo_model(snap);

    // Newest-first under the pinned "<current>" row: row 0 is <current>, the merge is
    // HEAD (row 1), root is last.
    assert!(model.commits[0].is_working, "row 0 is the pinned <current> row");
    assert_eq!(model.commits[1].hash, built.merge, "merge is HEAD/row 1");
    assert_eq!(model.commits.last().unwrap().hash, built.c1, "root is last");
    // The merge has two parents (c2 mainline, f1 side).
    assert_eq!(model.commits[1].parents.len(), 2, "merge has two parents");
    assert_eq!(model.commits[1].parents[0], built.c2, "first parent is mainline c2");
    assert!(model.commits[1].parents.contains(&built.f1), "second parent is f1");
    assert!(model.commits.last().unwrap().parents.is_empty(), "root has no parents");

    // The graph lays out >=2 lanes for the merge, and every edge is a one-lane shift
    // (no floating diagonals).
    assert!(model.graph.max_lanes >= 2, "the merge widens the gutter to >=2 lanes");
    for row in &model.graph.rows {
        for edge in &row.edges {
            assert_eq!(
                (edge.from as isize - edge.to as isize).abs(),
                1,
                "every graph edge is a single-lane shift (connected, no floating diagonal)"
            );
        }
    }
    // The merge row itself carries an edge (a connected merge glyph, not isolated).
    // Row 0 is now the pinned <current> tip; the merge is row 1.
    assert!(!model.graph.rows[1].edges.is_empty(), "the merge row has connector edges");
}

#[test]
fn select_commit_swaps_detail_to_new_real_hash() {
    let built = build_repo();
    let mut state = state_for(&built);

    // Default selection is the pinned "<current>" row (working summary, blank hash).
    assert_eq!(state.selected_commit_hash().as_deref(), Some(gitgit::model::WORKING_REV));
    assert_eq!(
        state.repo.detail.as_ref().map(|d| d.short_hash.clone()),
        Some(String::new()),
        "the <current> row has no commit hash in its detail"
    );

    // Move down one: selection + detail rebuild to HEAD (the merge)'s real hash.
    state.apply(Msg::Move(1));
    assert_eq!(state.selected_commit_hash().as_deref(), Some(built.merge.as_str()), "Move lands on HEAD");
    assert_eq!(
        state.repo.detail.as_ref().unwrap().short_hash,
        built.merge,
        "detail rebuilt to HEAD's real hash"
    );

    // Move down again: another real hash.
    state.apply(Msg::Move(1));
    let sel = state.selected_commit_hash().unwrap();
    assert_ne!(sel, built.merge, "Move changed the selected commit");
    assert_eq!(
        state.repo.detail.as_ref().unwrap().short_hash,
        sel,
        "detail rebuilt to the newly-selected commit's hash"
    );
    assert_eq!(state.view.focus, Pane::Log, "Move keeps Log focus");

    // PageDown clamps to the last (root) commit.
    state.apply(Msg::Move(100));
    assert_eq!(state.selected_commit_hash().as_deref(), Some(built.c1.as_str()), "clamps to root");
}

// -- CLUSTER B: per-commit changed-files tree --------------------------------

#[test]
fn select_commit_swaps_tree_with_real_added_modified_deleted_statuses() {
    let built = build_repo();
    let mut state = state_for(&built);

    // Select c2 (the add/modify/delete commit) and feed its real changed-files tree
    // through the loader seam (Msg::TreeLoaded), exactly as the runtime does.
    let c2_row = visible_commits(&state.repo, &state.view)
        .iter()
        .position(|&i| state.repo.commits[i].hash == built.c2)
        .unwrap();
    state.apply(Msg::SelectCommit(c2_row));
    assert_eq!(state.selected_commit_hash().as_deref(), Some(built.c2.as_str()));

    let tree = built.backend.changed_files(&built.c2).unwrap();
    let changed = state.apply(Msg::TreeLoaded { hash: built.c2.clone(), tree, ignored: Default::default() });
    assert!(changed, "matching-hash tree swap redraws");

    let files = flat_files(&state.repo.tree);
    assert_eq!(status_of(&files, "a.go"), Some(FileStatus::Modified), "a.go modified");
    assert_eq!(status_of(&files, "b.go"), Some(FileStatus::Added), "b.go added");
    assert_eq!(status_of(&files, "gone.go"), Some(FileStatus::Deleted), "gone.go deleted");
}

#[test]
fn stale_tree_loaded_is_dropped() {
    let built = build_repo();
    let mut state = state_for(&built);
    // Selection is HEAD (merge). A TreeLoaded for the (non-selected) root c1 is
    // ignored by the hash-staleness guard.
    let before = state.files_rows_len();
    let root_tree = built.backend.changed_files(&built.c1).unwrap();
    let changed = state.apply(Msg::TreeLoaded { hash: built.c1.clone(), tree: root_tree, ignored: Default::default() });
    assert!(!changed, "a tree for a non-selected commit is dropped");
    assert_eq!(state.files_rows_len(), before, "tree unchanged by the stale push");
}

#[test]
fn file_status_colors_render_on_real_data() {
    let built = build_repo();
    let mut state = state_for(&built);

    // Put c2's real mixed-status tree into the model, then render the files panel.
    let c2_row = visible_commits(&state.repo, &state.view)
        .iter()
        .position(|&i| state.repo.commits[i].hash == built.c2)
        .unwrap();
    state.apply(Msg::SelectCommit(c2_row));
    state.repo.tree = built.backend.changed_files(&built.c2).unwrap();
    // Hide the diff viewer so a file name appears ONLY in the files panel (the diff
    // header/body also prints the path), then expand so every file row is visible.
    state.apply(Msg::ToggleDiff);
    // Expand every directory so all file rows are visible (the toolbar Expand/Collapse
    // buttons were replaced by Flat; the model helper still drives full expansion).
    let _ = TreeNode::set_all_expanded(&mut state.repo.tree, true);

    let buf = render(&state, 200, 60);
    assert_eq!(fg_of_text(&buf, "a.go"), Some(Theme::LINK), "Modified file name is LINK blue");
    assert_eq!(fg_of_text(&buf, "b.go"), Some(Theme::ACCENT_RUN), "Added file name is ACCENT_RUN green");
    assert_eq!(fg_of_text(&buf, "gone.go"), Some(Theme::ACCENT_CLOSE), "Deleted file name is ACCENT_CLOSE red");
}

#[test]
fn fold_unfold_and_collapsed_chain_path_resolution() {
    let built = build_repo();
    let mut state = state_for(&built);

    // Use c2's tree which contains a collapsed `sub/` chain leaf (sub/gone.go deleted).
    state.repo.tree = built.backend.changed_files(&built.c2).unwrap();
    state.view.focus = Pane::Files;
    let _ = TreeNode::set_all_expanded(&mut state.repo.tree, true);
    let expanded_rows = state.files_rows_len();

    // A leaf under the (expanded) sub chain resolves to its FULL repo path.
    let sub_leaf = TreeNode::flatten(&state.repo.tree)
        .iter()
        .position(|r| matches!(&r.node, FlatKind::File { name, .. } if name == "gone.go"))
        .unwrap();
    let full = TreeNode::path_at(&state.repo.tree, sub_leaf).unwrap();
    assert_eq!(full, "sub/gone.go", "collapsed-chain leaf resolves to its full path");
    // A deleted file under that chain still has a real preview (diff vs parent).
    assert!(
        built.backend.file_view(&built.c2, &full).unwrap().is_some(),
        "deleted collapsed-chain file has a diff preview"
    );

    // ToggleExpand on the `sub` directory row collapses the tree (fewer rows). The flatten
    // index maps 1:1 to the visible row (no synthetic root row).
    let dir_row = TreeNode::flatten(&state.repo.tree)
        .iter()
        .position(|r| matches!(&r.node, FlatKind::Dir { .. }))
        .unwrap();
    state.view.files_sel = dir_row;
    state.apply(Msg::ToggleExpand);
    assert!(state.files_rows_len() < expanded_rows, "ToggleExpand collapsed a directory");
    // ClickFile on the same dir row toggles it back open (restores rows).
    state.apply(Msg::ClickFile(dir_row));
    assert_eq!(state.files_rows_len(), expanded_rows, "ClickFile re-expanded the directory");
    // ClickFile on a FILE row does NOT fold anything (row count stable).
    let file_row = TreeNode::flatten(&state.repo.tree)
        .iter()
        .position(|r| matches!(&r.node, FlatKind::File { .. }))
        .unwrap();
    state.apply(Msg::ClickFile(file_row));
    assert_eq!(state.files_rows_len(), expanded_rows, "ClickFile on a file row does not fold");
}

// -- CLUSTER C: diff viewer --------------------------------------------------

#[test]
fn diff_line_numbers_and_bands_on_real_modified_file() {
    let built = build_repo();
    let view = built.backend.file_view(&built.c2, "a.go").unwrap().unwrap();
    let FileView::Diff(diff) = view else {
        panic!("a changed file yields a Diff view");
    };
    // 1-based old/new line numbers: Context carries both, Added new-only, Removed old-only.
    assert!(
        diff.lines.iter().any(|l| l.kind == LineKind::Context && l.old_no.is_some() && l.new_no.is_some()),
        "context lines carry both line numbers"
    );
    assert!(
        diff.lines.iter().any(|l| l.kind == LineKind::Added && l.new_no.is_some() && l.old_no.is_none()),
        "added lines carry only a new number"
    );
    assert!(
        diff.lines.iter().any(|l| l.kind == LineKind::Removed && l.old_no.is_some() && l.new_no.is_none()),
        "removed lines carry only an old number"
    );

    // Rendered side-by-side: both add and del bands appear.
    let state = state_for(&built);
    let mut state = with_preview(state, &built, &built.c2, "a.go");
    let buf = render(&state, 200, 60);
    assert!(any_bg(&buf, Theme::DIFF_ADD_BG), "added band present (side-by-side)");
    assert!(any_bg(&buf, Theme::DIFF_DEL_BG), "removed band present (side-by-side)");

    // Toggle to unified: the render changes and the diff bands still appear.
    let before = whole_text(&buf);
    state.apply(Msg::ToggleDiffMode);
    assert_eq!(state.view.diff_mode, DiffMode::Unified);
    let unified = render(&state, 200, 60);
    assert_ne!(whole_text(&unified), before, "unified render differs from side-by-side");
    assert!(any_bg(&unified, Theme::DIFF_ADD_BG) && any_bg(&unified, Theme::DIFF_DEL_BG), "unified keeps both bands");
}

#[test]
fn inline_word_highlight_renders_on_real_modified_line() {
    let built = build_repo();
    // a.go's modified line shares a common prefix/suffix, so the backend marks the
    // changed middle span as inline_hl (the fix for the fixture-only inline gap).
    let view = built.backend.file_view(&built.c2, "a.go").unwrap().unwrap();
    let FileView::Diff(diff) = view else { panic!("Diff") };
    assert!(
        diff.lines.iter().any(|l| l.inline_hl.is_some()),
        "a real modified line carries an inline-changed span"
    );

    // The stronger inline band paints on the rendered diff.
    let state = with_preview(state_for(&built), &built, &built.c2, "a.go");
    let buf = render(&state, 200, 60);
    assert!(
        any_bg(&buf, Theme::INLINE_ADD) || any_bg(&buf, Theme::INLINE_DEL),
        "the inline word-highlight band renders on the real modified line"
    );
}

#[test]
fn source_preview_runs_tokenize_highlight_on_real_unchanged_file() {
    let built = build_repo();
    // keep.txt is unchanged in c2 -> a Source preview; route it through the loader's
    // highlight pass (tokenize::highlight) as the runtime does.
    let raw = built.backend.file_view(&built.c2, "keep.txt").unwrap().unwrap();
    let highlighted = gitgit::tokenize::highlight(raw);
    let FileView::Source(src) = highlighted else { panic!("unchanged file -> Source") };
    assert!(!src.lines.is_empty(), "source preview has lines");

    // A real Rust source preview yields >=2 distinct token kinds (highlight ran, not
    // a flat all-Ident fallback). a.go-style code; use b.go which has keywords.
    let rust = built.backend.file_view(&built.merge, "feature.go").unwrap();
    if let Some(view) = rust {
        let view = gitgit::tokenize::highlight(view);
        let kinds = distinct_token_kinds(&view);
        assert!(kinds >= 2, "tokenize::highlight produced multiple token kinds on real source/diff");
    }
}

#[test]
fn toggle_whitespace_word_wrap_and_show_diff_on_real_data() {
    let built = build_repo();
    let base = with_preview(state_for(&built), &built, &built.c2, "a.go");

    // ToggleDiff hides the diff region; the render changes.
    let mut s = base_clone(&base);
    let shown = render(&s, 160, 50);
    s.apply(Msg::ToggleDiff);
    assert!(!s.view.show_diff, "ToggleDiff flips show_diff off");
    let hidden = render(&s, 160, 50);
    assert_ne!(whole_text(&shown), whole_text(&hidden), "hiding the diff changes the frame");

    // ToggleWordWrap + ToggleWhitespace flip their view flags (no panic on real data).
    let mut s = base_clone(&base);
    s.apply(Msg::ToggleWordWrap);
    assert!(s.view.word_wrap, "ToggleWordWrap flips word_wrap on");
    s.apply(Msg::ToggleWhitespace);
    assert!(s.view.show_whitespace, "ToggleWhitespace flips show_whitespace on");
    let _ = render(&s, 100, 40); // renders without panic with both on
}

#[test]
fn deleted_and_missing_file_previews_do_not_panic() {
    let built = build_repo();
    // Deleted file: all-Removed diff vs parent, renders in both modes without panic.
    let mut state = with_preview(state_for(&built), &built, &built.c2, "sub/gone.go");
    let _ = render(&state, 120, 40);
    state.apply(Msg::ToggleDiffMode);
    let _ = render(&state, 120, 40);
    // A path absent at the commit -> no preview (None), renders the empty viewer.
    assert!(built.backend.file_view(&built.c2, "nope.go").unwrap().is_none());
}

// -- CLUSTER D: commit detail pane -------------------------------------------

#[test]
fn detail_pane_renders_real_author_committer_date_and_committed_by_line() {
    let built = build_repo();
    let mut state = state_for(&built);

    // Select f1 (author Bob, committer Alice) and load its real detail.
    let f1_row = visible_commits(&state.repo, &state.view)
        .iter()
        .position(|&i| state.repo.commits[i].hash == built.f1)
        .unwrap();
    state.apply(Msg::SelectCommit(f1_row));
    let detail = built.backend.commit_detail(&built.f1).unwrap();
    state.apply(Msg::DetailLoaded { hash: built.f1.clone(), detail });

    let buf = render(&state, 200, 60);
    let text = whole_text(&buf);
    assert!(text.contains("Bob Maint"), "author name renders");
    assert!(text.contains("bob@example.com"), "author email renders");
    assert!(text.contains("committed by") && text.contains("Alice Dev"), "committed-by line shows when committer != author");

    // HEAD (the merge) has author == committer -> NO committed-by line.
    let mut head = state_for(&built);
    let merge_detail = built.backend.commit_detail(&built.merge).unwrap();
    head.apply(Msg::DetailLoaded { hash: built.merge.clone(), detail: merge_detail });
    let head_text = whole_text(&render(&head, 200, 60));
    assert!(!head_text.contains("committed by"), "no committed-by line when author == committer");
}

#[test]
fn containing_branches_from_real_refs_and_branch_list_toggle() {
    let built = build_repo();
    let mut state = state_for(&built);

    // The root c1 is contained by every branch (default, feature, release): >=3.
    let c1_row = visible_commits(&state.repo, &state.view)
        .iter()
        .position(|&i| state.repo.commits[i].hash == built.c1)
        .unwrap();
    state.apply(Msg::SelectCommit(c1_row));
    let detail = built.backend.commit_detail(&built.c1).unwrap();
    let n = detail.containing_branches.len();
    assert!(n >= 3, "root is contained by the default, feature and release branches (got {n})");
    state.apply(Msg::DetailLoaded { hash: built.c1.clone(), detail });

    // Collapsed: a single "In N branches" line; expanded: one branch per line.
    let collapsed = whole_text(&render(&state, 120, 60));
    assert!(collapsed.contains(&format!("In {n} branches")), "plural containment header");
    state.apply(Msg::ToggleBranchList);
    assert!(state.view.branches_expanded);
    let expanded = whole_text(&render(&state, 120, 60));
    assert!(expanded.contains("feature") && expanded.contains("release"), "expanded list shows real branch names");
}

// -- CLUSTER E: search + filters ---------------------------------------------

#[test]
fn search_filters_by_subject_author_and_hash_case_insensitive_with_regex() {
    let built = build_repo();
    let mut state = state_for(&built);
    // The pinned "<current>" row is search-exempt (always visible), so every count
    // below is the real-match count PLUS that one row.
    let total = state.visible_len();
    assert_eq!(total, 5, "the <current> row + four commits (root, delta, feat, merge)");

    // Subject substring.
    push_search(&mut state, "delta");
    assert_eq!(state.visible_len(), 2, "subject 'delta' matches one commit (+ <current>)");
    clear_search(&mut state);
    assert_eq!(state.visible_len(), total, "clearing restores the full list");

    // Author substring (Bob authored f1).
    push_search(&mut state, "Bob");
    assert_eq!(state.visible_len(), 2, "author 'Bob' matches the feature commit (+ <current>)");
    clear_search(&mut state);

    // Hash prefix (4-char prefix of c2).
    push_search(&mut state, &built.c2[..4]);
    assert_eq!(state.visible_len(), 2, "hash prefix matches its commit (+ <current>)");
    clear_search(&mut state);

    // Case is ALWAYS ignored (no case toggle): 'DELTA' matches 'delta'.
    push_search(&mut state, "DELTA");
    assert_eq!(state.visible_len(), 2, "matching is case-insensitive: 'DELTA' matches 'delta' (+ <current>)");
    clear_search(&mut state);

    // Regex: 'd.lta' matches via regex but not as a substring; invalid '[' is safe.
    // Regex is also case-insensitive, so 'D.LTA' would match too.
    push_search(&mut state, "d.lta");
    assert_eq!(state.visible_len(), 1, "as a substring 'd.lta' matches nothing real (only <current>)");
    state.apply(Msg::ToggleRegex);
    assert_eq!(state.visible_len(), 2, "as a regex 'd.lta' matches 'delta' (+ <current>)");
    clear_search(&mut state);
    push_search(&mut state, "[");
    assert_eq!(state.visible_len(), 1, "an invalid regex yields no real matches (only <current>, no panic)");
}

#[test]
fn user_dropdown_options_from_real_authors_and_pick_filters() {
    let built = build_repo();
    let mut state = state_for(&built);
    let opts = gitgit::model::filter_options(&state.repo, FilterKind::User);
    assert_eq!(
        opts,
        vec!["All".to_string(), "<me>".to_string(), "Alice Dev".to_string(), "Bob Maint".to_string()],
        "All, then the dynamic <me>, then the distinct sorted real authors"
    );

    // Open + pick Bob -> only the feature commit; pick All (row 0) -> restore.
    state.apply(Msg::OpenDropdown(FilterKind::User));
    let bob_row = opts.iter().position(|o| o == "Bob Maint").unwrap();
    state.apply(Msg::DropdownPick(bob_row));
    assert_eq!(state.visible_len(), 2, "picking Bob filters to his one commit (+ <current>)");
    state.apply(Msg::OpenDropdown(FilterKind::User));
    state.apply(Msg::DropdownPick(0));
    assert_eq!(state.visible_len(), 5, "row 0 (All) clears the user filter (+ <current>)");

    // Picking "<me>" filters by the local identity (is_me); the search/filter-exempt
    // <current> row stays visible, so the count is at least 1 and never panics.
    state.apply(Msg::OpenDropdown(FilterKind::User));
    let me_row = opts.iter().position(|o| o == "<me>").unwrap();
    assert_eq!(me_row, 1, "<me> is the first selectable user");
    state.apply(Msg::DropdownPick(me_row));
    assert!(state.visible_len() >= 1, "the <me> filter keeps the exempt <current> row");
}

#[test]
fn date_dropdown_presets_filter_real_commit_dates() {
    let built = build_repo();
    let mut state = state_for(&built);
    let opts = gitgit::model::filter_options(&state.repo, FilterKind::Date);
    assert_eq!(opts[0], "All");
    assert!(opts.contains(&"Last 7 days".to_string()));

    // Anchored to the newest commit (the merge, day 6): the feature (day 4) and
    // delta (day 3) fall within 7 days; the root (day 0) does not.
    pick_option(&mut state, FilterKind::Date, "Last 7 days");
    assert_eq!(state.visible_len(), 4, "merge + feat + delta within 7 days (+ <current>)");
    pick_option(&mut state, FilterKind::Date, "Last 24 hours");
    assert_eq!(state.visible_len(), 2, "only the newest commit within 24h (+ <current>)");
    pick_all(&mut state, FilterKind::Date);
    assert_eq!(state.visible_len(), 5, "clearing the date filter restores all (+ <current>)");
}

#[test]
fn date_filter_works_under_iso_date_format() {
    // REGRESSION: the Date filter must work when [behavior].date_format = "iso"
    // (parse_date previously hard-coded the DD.MM.YYYY shape).
    let built = build_repo();
    let iso = Config::from_toml_str("[behavior]\ndate_format = \"iso\"\n").unwrap();
    let mut state = gitgit::bootstrap_real(&built.tmp.dir, &iso).unwrap();
    // The dates are ISO-formatted now; the same presets must still bucket them.
    // commits[0] is the pinned <current> row (blank date); the real HEAD is row 1.
    assert!(state.repo.commits[1].date.contains('-'), "dates render in ISO shape");
    pick_option(&mut state, FilterKind::Date, "Last 7 days");
    assert_eq!(state.visible_len(), 4, "ISO dates still bucket into 'Last 7 days' (+ <current>)");
    pick_all(&mut state, FilterKind::Date);
    assert_eq!(state.visible_len(), 5, "ISO date filter clears back to all (+ <current>)");
}

#[test]
fn branch_filter_selects_full_history_not_just_the_tip() {
    // REGRESSION (two bugs): the Branch filter must (1) select a branch's whole
    // reachable history, not only its tip ref, and (2) list a buried-tip branch.
    let built = build_repo();
    let mut state = state_for(&built);

    let opts = gitgit::model::filter_options(&state.repo, FilterKind::Branch);
    // `release` is pinned at c2 (a buried, non-HEAD tip) yet must appear in the list.
    assert!(opts.contains(&"release".to_string()), "a buried-tip branch still appears in the dropdown");
    assert!(opts.contains(&"feature".to_string()), "the feature branch appears");
    assert!(opts.iter().any(|o| o == &built.default_branch), "the default branch appears");

    // Picking the default branch selects its WHOLE history (all 4 commits), not 1.
    pick_option(&mut state, FilterKind::Branch, &built.default_branch);
    assert_eq!(state.visible_len(), 5, "the default branch contains all four loaded commits (+ <current>)");

    // Picking `feature` selects exactly its reachable commits: f1 + its ancestors
    // c2, c1 (3) - NOT the merge, which is not on the feature branch.
    pick_option(&mut state, FilterKind::Branch, "feature");
    let vis = visible_commits(&state.repo, &state.view);
    let hashes: Vec<&str> = vis.iter().map(|&i| state.repo.commits[i].hash.as_str()).collect();
    assert!(hashes.contains(&built.f1.as_str()) && hashes.contains(&built.c2.as_str()) && hashes.contains(&built.c1.as_str()), "feature history includes f1, c2, c1");
    assert!(!hashes.contains(&built.merge.as_str()), "the merge is NOT on the feature branch");
}

#[test]
fn filtering_reclamps_selection_and_rebuilds_detail() {
    let built = build_repo();
    let mut state = state_for(&built);
    // Park the selection at the last (root) row, then apply a 1-match search.
    state.apply(Msg::Move(100));
    assert_eq!(state.selected_commit_hash().as_deref(), Some(built.c1.as_str()));
    push_search(&mut state, "Merge");
    assert_eq!(state.visible_len(), 2, "only the merge matches (+ the pinned <current>)");
    assert_eq!(state.view.log_sel, 1, "log_sel re-clamped onto the merge (row 1, under <current>)");
    assert_eq!(
        state.repo.detail.as_ref().unwrap().short_hash,
        built.merge,
        "detail rebuilt to the single visible real commit"
    );
}

// -- CLUSTER F: startup / loading / error ------------------------------------

#[test]
fn non_git_dir_yields_a_backend_error() {
    let tmp = TempRepo::new(); // an empty, non-git directory
    let err = gitgit::open_real_backend(&tmp.dir, &Config::default());
    assert!(err.is_err(), "opening a non-git dir errors");
    // The runtime path: bootstrap_loading + apply(BackendError) reaches Status::Error
    // while quit stays false and the frame keeps rendering.
    let mut state = gitgit::bootstrap_loading(&Config::default());
    state.apply(Msg::BackendError("not a git repository".to_string()));
    assert!(matches!(state.status, Status::Error(_)), "error status set");
    assert!(!state.quit, "an error is non-fatal");
    let text = whole_text(&render_status(&state, 120, 40, &state.status));
    assert!(text.contains("Error:") && text.contains("not a git repository"), "error line renders");
}

#[test]
fn loading_then_ready_progressive_fill_on_real_data() {
    let built = build_repo();
    // Frame 1: the non-blocking Loading shell over an empty repo.
    let mut state = gitgit::bootstrap_loading(&Config::default());
    assert!(matches!(state.status, Status::Loading));
    assert!(state.repo.commits.is_empty(), "Loading shell starts empty");
    let loading = whole_text(&render_status(&state, 120, 40, &state.status));
    assert!(loading.contains("Loading history..."), "Loading shows the history placeholder");

    // RepoLoaded flips to Ready with the real commit count + rebuilt detail.
    let snap = built.backend.load_repo().unwrap();
    let model = gitgit::backend::build_repo_model(snap);
    state.apply(Msg::RepoLoaded(Box::new(model)));
    assert!(matches!(state.status, Status::Ready));
    assert_eq!(state.visible_len(), 5, "the <current> row + four real commits fill the log");
    assert_eq!(
        state.selected_commit_hash().as_deref(),
        Some(gitgit::model::WORKING_REV),
        "startup opens on the pinned <current> row"
    );
}

#[test]
fn revert_flow_modal_is_confirmation_gated_and_zero_io() {
    // The apply layer is ZERO-IO: RequestRevert opens the modal, ConfirmRevert only
    // PARKS the request in `view.pending_revert` (the runtime, not apply, does the
    // write). The button still renders + is wired.
    let built = build_repo();
    let mut state = state_on_c2(&built);

    // 'Revert' moved off the files toolbar into the Editor menu.
    assert!(!whole_text(&render(&state, 200, 60)).contains("Revert"), "no Revert toolbar button");
    state.apply(Msg::OpenMenu(MenuId::Editor));
    assert!(whole_text(&render(&state, 200, 60)).contains("Revert"), "Revert is in the Editor menu");
    state.apply(Msg::CloseMenu);

    // No marks -> the cursor file is the lone target; RequestRevert opens a 1-file
    // modal naming that path. No working-tree write happened in apply.
    let cursor_path = state.selected_file_path().expect("a file is selected");
    assert!(state.apply(Msg::RequestRevert), "RequestRevert redraws");
    let req = state.view.revert_confirm.clone().expect("modal opened");
    assert_eq!(req.paths, vec![cursor_path.clone()], "target is the cursor file");
    assert!(state.view.queued_revert().is_none(), "apply does NOT dispatch IO");

    // The modal renders a Yes/No box over the file name.
    let modal_text = whole_text(&render(&state, 200, 60));
    assert!(modal_text.contains("Revert Selected Changes"), "modal title renders");
    assert!(modal_text.contains("[Yes]") && modal_text.contains("[No]"), "Yes/No buttons render");

    // Cancel closes the modal with nothing parked.
    let mut cancelled = state_on_c2(&built);
    cancelled.apply(Msg::RequestRevert);
    assert!(cancelled.apply(Msg::CancelRevert), "CancelRevert closes the modal");
    assert!(cancelled.view.revert_confirm.is_none() && cancelled.view.queued_revert().is_none());

    // Confirm closes the modal and parks the SAME request for the runtime to send.
    assert!(state.apply(Msg::ConfirmRevert), "ConfirmRevert redraws");
    assert!(state.view.revert_confirm.is_none(), "modal closed on confirm");
    assert_eq!(
        state.view.queued_revert().map(|r| r.paths.clone()),
        Some(vec![cursor_path]),
        "the confirmed request is parked for the runtime"
    );
}

#[test]
fn revert_request_with_marks_targets_all_marked_paths() {
    // With a non-empty marked set, RequestRevert targets EVERY marked path (not the
    // cursor); the modal lists them all.
    let built = build_repo();
    let mut state = state_on_c2(&built);
    // Mark two distinct file rows by path (path-keyed set, robust to index).
    let paths: Vec<String> = (0..state.files_rows_len())
        .filter_map(|i| TreeNode::path_at(&state.repo.tree, i))
        .take(2)
        .collect();
    assert_eq!(paths.len(), 2, "the tree has at least two files to mark");
    for p in &paths {
        state.view.files_marked.insert(p.clone());
    }
    assert!(state.apply(Msg::RequestRevert), "RequestRevert opens the modal");
    let mut got = state.view.revert_confirm.clone().expect("modal").paths;
    got.sort();
    let mut want = paths.clone();
    want.sort();
    assert_eq!(got, want, "the modal targets all marked paths");
}

#[test]
fn revert_request_with_nothing_selected_shows_hint_no_modal() {
    // A directory cursor with no marks -> a status hint, NO modal.
    let built = build_repo();
    let mut state = state_on_c2(&built);
    // Park the cursor on a DIRECTORY row (no resolvable file path) with no marks.
    let dir_row = (0..state.files_rows_len())
        .find(|&i| TreeNode::path_at(&state.repo.tree, i).is_none())
        .expect("c2's tree has a directory row");
    state.apply(Msg::SelectFile(dir_row));
    state.view.files_marked.clear();
    assert!(state.selected_file_path().is_none(), "the cursor is on a directory (no file path)");
    assert!(state.apply(Msg::RequestRevert), "RequestRevert still redraws (sets the hint)");
    assert!(state.view.revert_confirm.is_none(), "no modal opens with nothing to revert");
    assert!(matches!(state.status, Status::Notice(_)), "a status hint is shown");
}

#[test]
fn backend_revert_file_modified_added_deleted_and_root() {
    // Prove the REAL working-tree revert (whole-file vs parent[0]) on the tempdir
    // repo - NEVER on the project repo. c2 modifies a.go, adds b.go, deletes
    // sub/gone.go (vs its parent c1).
    let built = build_repo();
    let backend = &built.backend;
    let root = built.tmp.dir.clone();

    // Pre-state: the merge is checked out, so a.go has c2's body, b.go exists,
    // sub/gone.go is absent.
    let a_before = std::fs::read_to_string(root.join("a.go")).unwrap();
    assert!(a_before.contains("return"), "a.go currently holds the modified body");

    // MODIFIED in c2 -> overwrite with the parent (c1) content.
    let out = backend.revert_file(&built.c2, "a.go").unwrap();
    assert!(matches!(out, gitgit::backend::RevertOutcome::Overwritten(_)), "a.go overwritten");
    let a_after = std::fs::read_to_string(root.join("a.go")).unwrap();
    assert_eq!(a_after, "package main\n\nfunc A() {}\n", "a.go restored to the parent body");

    // ADDED in c2 (absent in c1) -> deleted from the working tree.
    assert!(root.join("b.go").exists(), "b.go exists before revert");
    let out = backend.revert_file(&built.c2, "b.go").unwrap();
    assert!(matches!(out, gitgit::backend::RevertOutcome::Deleted(_)), "b.go deleted");
    assert!(!root.join("b.go").exists(), "b.go removed from the working tree");

    // DELETED in c2 (present in c1) -> restored from the parent blob.
    assert!(!root.join("sub/gone.go").exists(), "gone.go absent before revert");
    let out = backend.revert_file(&built.c2, "sub/gone.go").unwrap();
    assert!(matches!(out, gitgit::backend::RevertOutcome::Restored(_)), "gone.go restored");
    assert_eq!(
        std::fs::read_to_string(root.join("sub/gone.go")).unwrap(),
        "package sub\n",
        "gone.go restored to the parent blob"
    );

    // ROOT commit: a.go was ADDED at c1 (parent = empty tree) -> reverting deletes it.
    let out = backend.revert_file(&built.c1, "a.go").unwrap();
    assert!(matches!(out, gitgit::backend::RevertOutcome::Deleted(_)), "root-added file deleted");
    assert!(!root.join("a.go").exists(), "a.go removed (root add reverted vs empty parent)");
}

#[test]
fn backend_revert_rejects_traversal_and_batches_two_files() {
    let built = build_repo();
    let backend = &built.backend;
    let root = built.tmp.dir.clone();

    // Workdir-escaping paths are rejected (no write outside the repo).
    assert!(backend.revert_file(&built.c2, "../escape.go").is_err(), "parent traversal rejected");
    assert!(backend.revert_file(&built.c2, "/etc/passwd").is_err(), "absolute path rejected");

    // A 2-file batch reverts BOTH (modified a.go -> parent body; added b.go -> gone).
    backend.revert_file(&built.c2, "a.go").unwrap();
    backend.revert_file(&built.c2, "b.go").unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("a.go")).unwrap(),
        "package main\n\nfunc A() {}\n",
        "a.go reverted in the batch"
    );
    assert!(!root.join("b.go").exists(), "b.go reverted in the batch");
}

#[test]
fn backend_revert_bare_repo_errors() {
    // A bare repo has no working tree -> revert_file errors (no write target).
    let tmp = TempRepo::new();
    let bare = tmp.dir.join("bare.git");
    std::fs::create_dir_all(&bare).unwrap();
    Repository::init_bare(&bare).unwrap();
    let backend = gitgit::open_real_backend(&bare, &Config::default()).unwrap();
    assert!(
        backend.revert_file("HEAD", "a.go").is_err(),
        "a bare repo (no workdir) rejects revert"
    );
}

// -- small helpers -----------------------------------------------------------

fn flat_files(tree: &[TreeNode]) -> Vec<(String, FileStatus)> {
    let mut out = Vec::new();
    collect(tree, &mut out);
    out
}

fn collect(nodes: &[TreeNode], out: &mut Vec<(String, FileStatus)>) {
    for node in nodes {
        match node {
            TreeNode::Dir { children, .. } => collect(children, out),
            TreeNode::File { name, status } => out.push((name.clone(), *status)),
        }
    }
}

fn status_of(files: &[(String, FileStatus)], name: &str) -> Option<FileStatus> {
    files.iter().find(|(n, _)| n == name).map(|(_, s)| *s)
}

fn distinct_token_kinds(view: &FileView) -> usize {
    let mut kinds: Vec<gitgit::diff::TokenKind> = Vec::new();
    let mut note = |toks: &[gitgit::diff::Token]| {
        for t in toks {
            if !kinds.contains(&t.kind) {
                kinds.push(t.kind);
            }
        }
    };
    match view {
        FileView::Diff(d) => d.lines.iter().for_each(|l| note(&l.tokens)),
        FileView::Source(s) => s.lines.iter().for_each(|l| note(l)),
        FileView::Blame(b) => b.lines.iter().for_each(|l| note(&l.tokens)),
        // A binary view carries no tokens to count.
        FileView::Binary(_) => {}
    }
    kinds.len()
}

/// Build an AppState then load the real preview for (commit, path) via the loader
/// seam (Msg::PreviewLoaded), selecting that commit first so the staleness guard
/// accepts it.
fn with_preview(mut state: AppState, built: &Built, commit: &str, path: &str) -> AppState {
    let row = visible_commits(&state.repo, &state.view)
        .iter()
        .position(|&i| state.repo.commits[i].hash == commit)
        .unwrap();
    state.apply(Msg::SelectCommit(row));
    // Put that commit's tree in so the path resolves, and select the file row.
    state.repo.tree = built.backend.changed_files(commit).unwrap();
    state.view.focus = Pane::Files;
    let _ = gitgit::model::TreeNode::set_all_expanded(&mut state.repo.tree, true);
    // Locate the file in VISIBLE rows so `files_sel` points at the file and `PreviewLoaded`'s
    // staleness guard accepts the preview.
    let file_row = visible_file_rows(&state.repo, &state.view)
        .iter()
        .position(|(r, _)| matches!(&r.node, FlatKind::File { name, .. } if path.ends_with(name.as_str())))
        .unwrap();
    state.view.files_sel = file_row;
    let view = built.backend.file_view(commit, path).unwrap().map(gitgit::tokenize::highlight);
    state.apply(Msg::PreviewLoaded {
        commit: commit.to_string(),
        path: path.to_string(),
        view,
    });
    state
}

/// Clone an AppState's repo+view into a fresh state for independent toggling.
fn base_clone(state: &AppState) -> AppState {
    let mut s = AppState::from_repo(state.repo.clone());
    s.view = state.view.clone();
    s.repo.preview = state.repo.preview.clone();
    s
}

fn push_search(state: &mut AppState, s: &str) {
    state.apply(Msg::SearchFocus);
    for ch in s.chars() {
        state.apply(Msg::SearchPush(ch));
    }
}

fn clear_search(state: &mut AppState) {
    state.apply(Msg::SearchBlur { clear: true });
}

fn pick_option(state: &mut AppState, kind: FilterKind, label: &str) {
    let opts = gitgit::model::filter_options(&state.repo, kind);
    let row = opts.iter().position(|o| o == label).expect("option present");
    state.apply(Msg::OpenDropdown(kind));
    state.apply(Msg::DropdownPick(row));
}

fn pick_all(state: &mut AppState, kind: FilterKind) {
    state.apply(Msg::OpenDropdown(kind));
    state.apply(Msg::DropdownPick(0));
}
