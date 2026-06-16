//! The fixture git backend: hardcoded demo data shaped exactly like a real
//! backend's output. Moved verbatim from the old free-function fixture module,
//! now behind [`GitBackend`]. The `ui`/`store` layers never know it is fake.
//!
//! It pre-tokenizes its previews (its `FileView` is already syntax-highlighted),
//! so the default render stays byte-identical: there is no extra highlight pass
//! on the fixture path. The real backend instead returns raw lines and runs
//! `crate::tokenize` once - both converge on the same UI types.
//!
//! NOTE: the `is_me` flag is NOT set here; [`super::build_repo_model`] stamps it
//! from `RepoSnapshot::current_user`, the single derivation site. The fixture only
//! supplies the raw author + the current-user string.

use crate::diff::{DiffLine, FileDiff, FileView, LineKind, SourceFile, Token};
use crate::highlight::{highlight_block, highlight_line};
use crate::model::{
    detail_from, Commit, CommitDetail, FileStatus, Ref, RefKind, SubjectSpan, SubjectTone, TreeNode,
};

use super::{BackendError, GitBackend, RepoSnapshot, RevertOutcome};

/// The "logged in" user; their commits render bold in the log (JetBrains habit).
const CURRENT_USER: &str = "Alexander Sidko";

/// The fixture-backed git source. Resolves synchronously and never errors, so the
/// default startup path is fully deterministic.
pub(crate) struct FixtureBackend;

impl GitBackend for FixtureBackend {
    fn load_repo(&self) -> Result<RepoSnapshot, BackendError> {
        Ok(RepoSnapshot {
            commits: commits(),
            tree: file_tree(),
            default_selection: DEFAULT_TREE_SELECTION,
            current_user: CURRENT_USER.to_string(),
            unpushed: std::collections::HashSet::new(),
            has_remotes: false,
            truncated: false,
            status_sig: 0,
        })
    }

    fn commit_detail(&self, hash: &str) -> Result<CommitDetail, BackendError> {
        // The cheap detail is derived from the matching commit row (subject, hash,
        // author, containing branches). The real backend will enrich it with the
        // committer signature; the fixture reuses the author as committer.
        commits()
            .iter()
            .find(|c| c.hash == hash)
            .map(detail_from)
            .ok_or_else(|| BackendError(format!("no such commit: {hash}")))
    }

    fn file_view(&self, _commit: &str, path: &str) -> Result<Option<FileView>, BackendError> {
        // Keyed on PATH (the old filename-suffix heuristic, moved in here): an
        // unchanged `*_test.go` shows a single-pane source; any other `.go` shows
        // the representative analytics_error diff; non-Go -> no preview.
        let view = if path.ends_with("_test.go") {
            Some(analytics_error_test_src())
        } else if path.ends_with(".go") {
            Some(analytics_error_diff())
        } else {
            None
        };
        Ok(view)
    }

    fn changed_files(&self, _hash: &str) -> Result<Vec<TreeNode>, BackendError> {
        // The fixtures have ONE static changed-files tree; it is returned for any
        // commit so the demo's behavior is unchanged (the golden frame and the
        // load_repo default tree both keep using `file_tree`).
        Ok(file_tree())
    }

    fn full_tree(&self, _hash: &str) -> Result<Vec<TreeNode>, BackendError> {
        // A representative fuller tree for the demo's "All" toggle: the same changed
        // files plus a handful of Unchanged siblings in plausible dirs, so the demo
        // shows changed-colored rows next to plain Unchanged rows. Deterministic.
        Ok(full_file_tree())
    }

    fn revert_file(&self, _commit: &str, _path: &str) -> Result<RevertOutcome, BackendError> {
        // The fixture path is HERMETIC: a revert never touches disk. The demo flow
        // (modal -> confirm) still works end-to-end; the outcome is a no-op.
        Ok(RevertOutcome::Demo)
    }

    fn revert_hunk(&self, _commit: &str, _path: &str, _hunk: usize) -> Result<RevertOutcome, BackendError> {
        // HERMETIC: the demo hunk-revert is a no-op (nothing touches disk).
        Ok(RevertOutcome::Demo)
    }

    fn read_worktree(&self, path: &str) -> Result<String, BackendError> {
        // HERMETIC: never reads disk. A deterministic stub so the demo editor opens
        // with editable content (the path is echoed so it is obviously the fixture).
        Ok(format!(
            "// fixture working copy of {path}\nfn main() {{\n    println!(\"edit me\");\n}}\n"
        ))
    }

    fn write_worktree(&self, _path: &str, _content: &str) -> Result<(), BackendError> {
        // HERMETIC: the demo save is a no-op (nothing touches disk).
        Ok(())
    }

    fn read_commit_file(&self, _commit: &str, _path: &str) -> Result<Option<String>, BackendError> {
        // HERMETIC: a base that lacks the worktree stub's leading comment, so the demo
        // editable diff opens with one Added line (the comment) to show the live diff.
        Ok(Some("fn main() {\n    println!(\"edit me\");\n}\n".to_string()))
    }
}

/// One link-free subject span.
fn text(s: &str) -> SubjectSpan {
    SubjectSpan::plain(s)
}
fn link(s: &str) -> SubjectSpan {
    SubjectSpan {
        text: s.to_string(),
        tone: SubjectTone::Link,
    }
}

fn rref(name: &str, kind: RefKind) -> Ref {
    Ref {
        name: name.to_string(),
        kind,
    }
}

/// Build a commit row from its parts. `parents` are the short hashes the graph
/// engine routes lanes from (first = mainline, 2+ = a merge, none = root).
/// `is_me` is left `false` here and stamped later by `build_repo_model`.
fn commit(
    hash: &str,
    parents: Vec<&str>,
    subject: Vec<SubjectSpan>,
    refs: Vec<Ref>,
    author: &str,
    date: &str,
) -> Commit {
    Commit {
        hash: hash.to_string(),
        full_hash: hash.to_string(),
        parents: parents.into_iter().map(str::to_string).collect(),
        subject,
        refs,
        author: author.to_string(),
        date: date.to_string(),
        date_label: date.to_string(),
        is_me: false,
        head: false,
        containing_branches: containing_branches(hash),
        is_working: false,
        working: None,
    }
}

/// Branches that contain commit `hash`, for the detail panel's "In N branches"
/// block (a real backend fills this from `git branch --contains`). The trunk
/// `origin/develop` contains every demo commit; feature commits add their branch
/// so the expand/collapse is visible. The DOP-9923 base commit gets the three
/// branches from the reference design.
fn containing_branches(hash: &str) -> Vec<String> {
    let names: &[&str] = match hash {
        // DOP-9923 "[jenkins] add atdd declarative pipeline" base commit.
        "e5d66e36" => &[
            "origin/develop",
            "origin/feature/FEC-10428-post-processor-changes-for-ott-correlation",
            "origin/feature/SQA-28",
        ],
        "412238f4" | "e573e1a4" | "ccb5883c" | "0c42edc1" | "dbc3bd2c" => &[
            "origin/develop",
            "origin/feature/FEC-10428-post-processor-changes-for-ott-correlation",
        ],
        "376353f5" => &["origin/develop", "origin/feature/SQA-28"],
        "0a532eef" => &["origin/develop", "origin/feature/IPC-1555-Audit"],
        _ => &["origin/develop"],
    };
    names.iter().map(|s| s.to_string()).collect()
}

/// The demo commit log, newest first. Each commit declares its parent short
/// hashes; [`crate::graph_engine::build_layout`] derives the connected gutter from
/// that topology. The DAG reads as a green `develop` trunk (lane 0) that spawns
/// and reabsorbs side branches: an orange feature branch (with a teal sub-branch),
/// a magenta side, and a lower blue side. Every parent sits strictly below its
/// child, so the layout is a valid acyclic graph and each row carries one node.
fn commits() -> Vec<Commit> {
    use RefKind::*;
    let mut rows = vec![
        // 0: merge - first parent stays on the trunk, second spawns the orange branch.
        commit(
            "1af3bd63",
            vec!["e5d66e36", "412238f4"],
            vec![text("SQA-9: [hi2] unknown LIID fallback to unallocated trigger")],
            vec![rref("origin/develop", RemoteBranch)],
            CURRENT_USER,
            "22.05.2026, 12:08",
        ),
        // 1: orange tip - mainline continues, second parent spawns the teal sub-branch.
        commit(
            "412238f4",
            vec!["0c42edc1", "e573e1a4"],
            vec![text("Added missing mocks")],
            vec![rref("../feature/FEC-10428-decoder", RemoteBranch)],
            "Sharda Mallick",
            "25.05.2026, 07:51",
        ),
        // 2: teal branch.
        commit(
            "e573e1a4",
            vec!["376353f5"],
            vec![text("FEC-10428: Added per agency calculation")],
            vec![],
            "Sharda Mallick",
            "25.05.2026, 07:33",
        ),
        commit(
            "376353f5",
            vec!["ccb5883c"],
            vec![text("SQA-28: Added the required changes for HI2 decoding")],
            vec![rref("origin/feature/SQA-28", RemoteBranch)],
            "shail",
            "25.05.2026, 07:30",
        ),
        // 4: teal rejoins orange (its parent is the orange commit below).
        commit(
            "ccb5883c",
            vec!["0c42edc1"],
            vec![text("FEC-10428: Changes related to license")],
            vec![],
            "Sharda Mallick",
            "22.05.2026, 10:05",
        ),
        // 5: orange commit absorbing the teal sub-branch.
        commit(
            "0c42edc1",
            vec!["dbc3bd2c"],
            vec![text("Added configurations")],
            vec![],
            "Sharda Mallick",
            "21.05.2026, 09:18",
        ),
        commit(
            "dbc3bd2c",
            vec!["0a532eef"],
            vec![text("FEC-10428:[Post-processor] : Added OTT correlation and DB changes")],
            vec![],
            "Sharda Mallick",
            "21.05.2026, 06:27",
        ),
        // 7: orange tip - its parent is the trunk commit, so orange rejoins green.
        commit(
            "0a532eef",
            vec!["e5d66e36"],
            vec![text("Removed unwanted changes")],
            vec![rref("../feature/IPC-1555-Audit", RemoteBranch)],
            "Sharda Mallick",
            "21.05.2026, 06:20",
        ),
        // 8: trunk commit the orange branch merged back into.
        commit(
            "e5d66e36",
            vec!["bcd87e68"],
            vec![text("DOP-9923: [jenkins] add atdd declarative pipeline")],
            vec![],
            "Woo Wai Kee",
            "19.05.2026, 20:01",
        ),
        // 9: merge - mainline continues, second parent spawns the magenta side.
        commit(
            "bcd87e68",
            vec!["916e862a", "2e3c5506"],
            vec![text("IPC-1555:[Audit Log]: Implemented Audit log service.")],
            vec![],
            "Sharda Mallick",
            "19.05.2026, 16:41",
        ),
        // 10: magenta node - its parent is the trunk commit, so magenta rejoins green.
        commit(
            "2e3c5506",
            vec!["916e862a"],
            vec![text("IPC-1547: [nats] make jetstream file store configurable")],
            vec![],
            CURRENT_USER,
            "19.05.2026, 12:08",
        ),
        // 11: merge - mainline continues, second parent spawns the lower blue side.
        commit(
            "916e862a",
            vec!["c12f5e7d", "9c5e43b2"],
            vec![
                text("Merge branch 'develop' of "),
                link("https://web.datafusion.ai/stash/scm/mi/mcng-ip"),
                text(" int"),
            ],
            vec![],
            "ks07",
            "19.05.2026, 07:55",
        ),
        // 12: blue node - its parent is the trunk commit, so blue rejoins green.
        commit(
            "9c5e43b2",
            vec!["c12f5e7d"],
            vec![text("MCNG-24658 : Update version for Release 2.6.2")],
            vec![],
            "ks07",
            "19.05.2026, 07:33",
        ),
        // 13: trunk commit both lower sides merged back into.
        commit(
            "c12f5e7d",
            vec!["b2ffe5e0"],
            vec![text("SQA-9: [api] fix start/end time format error typo")],
            vec![],
            CURRENT_USER,
            "19.05.2026, 07:10",
        ),
        commit(
            "b2ffe5e0",
            vec!["aea6637b"],
            vec![text("SQA-9: [hi2] make target_location optional in Avro schema")],
            vec![],
            CURRENT_USER,
            "19.05.2026, 07:05",
        ),
        commit(
            "aea6637b",
            vec!["e5031990"],
            vec![text("SQA-9: [hi2] NFS flusher, WAL rotation, and startup recovery")],
            vec![],
            CURRENT_USER,
            "19.05.2026, 06:58",
        ),
        commit(
            "e5031990",
            vec!["5768a777"],
            vec![text("UI-1896:[IP-AgencyService]:Increase timeout and fix")],
            vec![rref("UI-1896-increase-timeout", LocalBranch)],
            "Sharda Mallick",
            "25.05.2026, 11:51",
        ),
        // 17: root - no parents, terminates the trunk.
        commit(
            "5768a777",
            vec![],
            vec![text("DOP-9923: [jenkins] add atdd declarative pipeline")],
            vec![rref("../feat/DOP-9923-atdd-pipeline", RemoteBranch)],
            "Woo Wai Kee",
            "25.05.2026, 11:50",
        ),
    ];
    rows[0].head = true; // HEAD -> origin/develop
    rows
}

// -- changed-files tree ----------------------------------------------------
use FileStatus::{Added, Deleted, Modified, Unchanged};

/// A changed file leaf with an explicit git status (drives its name color).
fn file_with(name: &str, status: FileStatus) -> TreeNode {
    TreeNode::File {
        name: name.to_string(),
        status,
    }
}

/// A modified file (the common case; blue name).
fn file(name: &str) -> TreeNode {
    file_with(name, Modified)
}

/// A newly added file (green name).
fn added(name: &str) -> TreeNode {
    file_with(name, Added)
}

/// A deleted file (red, struck-through name).
fn deleted(name: &str) -> TreeNode {
    file_with(name, Deleted)
}

/// An unchanged file (plain text; only shown in the All-files view).
fn unchanged(name: &str) -> TreeNode {
    file_with(name, Unchanged)
}

fn dir(name: &str, file_count: usize, children: Vec<TreeNode>) -> TreeNode {
    TreeNode::Dir {
        name: name.to_string(),
        file_count,
        expanded: true,
        children,
    }
}

/// Changed-files tree for the selected commit (1af3bd63). Path-collapsed root,
/// matching the GoLand "Directory" grouping with file counts.
fn file_tree() -> Vec<TreeNode> {
    vec![dir(
        "packages/transform/go/activity-handler/internal",
        11,
        vec![
            dir("app", 1, vec![file("app.go")]),
            dir("common", 1, vec![added("constants.go")]),
            dir(
                "metrics",
                2,
                vec![
                    dir("mocks", 1, vec![added("mock_metrics.go")]),
                    file("metrics.go"),
                ],
            ),
            dir(
                "service",
                7,
                vec![
                    dir(
                        "hi2_storage",
                        4,
                        vec![
                            file("hi2_storage.go"),
                            added("nfs_flusher_test.go"),
                            deleted("hi2_storage_test.go"),
                            file("wal_writer.go"),
                        ],
                    ),
                    file("handler.go"),
                    deleted("router_old.go"),
                    file("server.go"),
                ],
            ),
        ],
    )]
}

/// Index (into the flattened tree) of the row selected on startup: `app.go`.
const DEFAULT_TREE_SELECTION: usize = 2;

/// The demo's FULL ("All" toggle) tree: the changed files from [`file_tree`] plus
/// a handful of Unchanged siblings in plausible dirs, so the All view shows
/// status-colored rows next to plain Unchanged ones. Deterministic; the same
/// collapsed-root grouping the changed tree uses. The runtime collapses the dirs
/// on load (All mode opens collapsed), so this builds them as `dir` like the rest.
fn full_file_tree() -> Vec<TreeNode> {
    vec![dir(
        "packages/transform/go/activity-handler/internal",
        16,
        vec![
            dir("app", 2, vec![file("app.go"), unchanged("app_test.go")]),
            dir("common", 2, vec![added("constants.go"), unchanged("logging.go")]),
            dir(
                "metrics",
                3,
                vec![
                    dir("mocks", 1, vec![added("mock_metrics.go")]),
                    file("metrics.go"),
                    unchanged("metrics_test.go"),
                ],
            ),
            dir(
                "service",
                9,
                vec![
                    dir(
                        "hi2_storage",
                        5,
                        vec![
                            file("hi2_storage.go"),
                            added("nfs_flusher_test.go"),
                            deleted("hi2_storage_test.go"),
                            unchanged("reader.go"),
                            file("wal_writer.go"),
                        ],
                    ),
                    file("handler.go"),
                    deleted("router_old.go"),
                    file("server.go"),
                    unchanged("config.go"),
                ],
            ),
        ],
    )]
}

// -- diff / preview fixtures ----------------------------------------------

/// Build one tokenized diff line. `old_no`/`new_no` are 1-based or `None`.
fn diff_line(
    old_no: Option<usize>,
    new_no: Option<usize>,
    kind: LineKind,
    text: &str,
    inline_hl: Option<(usize, usize)>,
) -> DiffLine {
    DiffLine {
        old_no,
        new_no,
        kind,
        tokens: highlight_line("go", text),
        inline_hl,
        hunk: 0,
        fold: None,
    }
}

/// Convenience: a context line present on both sides at the same number.
fn ctx(no: usize, text: &str) -> DiffLine {
    diff_line(Some(no), Some(no), LineKind::Context, text, None)
}

/// The analytics_error.go diff: the typo fix NewAnalyticError -> NewAnalyticsError
/// on the const block. A leading fold marker stands in for the 3 omitted head lines; the
/// two changed const lines are a Removed/Added pair with an inline highlight on the
/// inserted "s". Raw text is auto-highlighted - no hand-authored token colors.
fn analytics_error_diff() -> FileView {
    let lines = vec![
        DiffLine::fold_marker(3),
        ctx(4, "\treturn Error{Message: message, Code: code}"),
        ctx(5, "}"),
        ctx(7, ""),
        diff_line(Some(8), Some(8), LineKind::Context, "var (", None),
        // First changed const: typo fix. Removed old, added new with inline "s".
        diff_line(
            Some(10),
            None,
            LineKind::Removed,
            "\tINVALIDSTARTTIMEFORMAT = NewAnalyticError(\"Invalid start time format, format YYYY-MM-DD\", 4001)",
            None,
        ),
        diff_line(
            None,
            Some(10),
            LineKind::Added,
            "\tINVALIDSTARTTIMEFORMAT = NewAnalyticsError(\"Invalid start time format, format YYYY-MM-DD\", 4001)",
            // Char offset of the inserted "s" in NewAnalyti(s)Error.
            Some((37, 38)),
        ),
        diff_line(
            Some(11),
            None,
            LineKind::Removed,
            "\tINVALIDENDTIMEFORMAT   = NewAnalyticError(\"Invalid end time format, format YYYY-MM-DD\", 4002)",
            None,
        ),
        diff_line(
            None,
            Some(11),
            LineKind::Added,
            "\tINVALIDENDTIMEFORMAT   = NewAnalyticsError(\"Invalid end time format, format YYYY-MM-DD\", 4002)",
            Some((37, 38)),
        ),
        ctx(12, "\tSTARTTIMEMISSING       = NewAnalyticError(\"Start time is missing\", 4002)"),
        ctx(13, "\tENDTIMEMISSING         = NewAnalyticError(\"End time is missing\", 4003)"),
        ctx(14, "\tACTIVITYTYPENOTFOUND   = NewAnalyticError(\"Activity type not found\", 4004)"),
        ctx(15, "\tTIMERANGETOOLONG       = NewAnalyticError(\"Time range is too long, maximum allowed is 31 days\", 4005)"),
    ];
    FileView::Diff(FileDiff {
        path: "packages/transform/go/ip-agency-service/internal/agencyerror/analytics_error.go"
            .to_string(),
        old_rev: "b2ffe5e0".to_string(),
        new_rev: "c12f5e7d".to_string(),
        lines,
    })
}

/// An unchanged file's source preview (single pane, no change bands), auto
/// syntax-highlighted as Go.
fn analytics_error_test_src() -> FileView {
    const SRC: &str = "\
package agencyerror

import \"testing\"

func TestNewAnalyticsError(t *testing.T) {
\terr := NewAnalyticsError(\"boom\", 4001)
\tif err.Code != 4001 {
\t\tt.Fatalf(\"expected 4001, got %d\", err.Code)
\t}
}";
    let lines: Vec<Vec<Token>> = highlight_block("go", SRC);
    FileView::Source(SourceFile {
        path: "packages/transform/go/ip-agency-service/internal/agencyerror/analytics_error_test.go"
            .to_string(),
        lang: "go".to_string(),
        lines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::build_repo_model;

    /// The known merge commit (row 0): two parents, on the trunk.
    const MERGE_HASH: &str = "1af3bd63";

    #[test]
    fn load_repo_shape() {
        let snap = FixtureBackend.load_repo().unwrap();
        assert_eq!(snap.commits.len(), 18, "fixture commit count");
        assert_eq!(snap.default_selection, DEFAULT_TREE_SELECTION);
        assert_eq!(snap.current_user, CURRENT_USER);
        // The merge row carries two parents (spawns a side branch).
        let merge = snap
            .commits
            .iter()
            .find(|c| c.hash == MERGE_HASH)
            .expect("merge commit present");
        assert_eq!(merge.parents.len(), 2, "merge has two parents");
    }

    #[test]
    fn build_repo_model_derives() {
        let snap = FixtureBackend.load_repo().unwrap();
        let n = snap.commits.len();
        let model = build_repo_model(snap);
        // The graph has exactly one row per commit (no recompute downstream).
        assert_eq!(model.graph.rows.len(), n, "one graph row per commit");
        assert!(model.detail.is_none(), "detail starts empty");
        assert!(model.preview.is_none(), "preview starts empty");
        // `is_me` is computed from current_user in build_repo_model, not the fixture.
        let me = model
            .commits
            .iter()
            .find(|c| c.author == CURRENT_USER)
            .expect("current user authored a commit");
        assert!(me.is_me, "author == current_user -> is_me");
        let other = model
            .commits
            .iter()
            .find(|c| c.author != CURRENT_USER)
            .expect("another author exists");
        assert!(!other.is_me, "other author -> not is_me");
    }

    #[test]
    fn commit_detail_has_branches() {
        let detail = FixtureBackend.commit_detail(MERGE_HASH).unwrap();
        assert!(
            !detail.containing_branches.is_empty(),
            "containing branches folded into commit_detail"
        );
        assert!(FixtureBackend.commit_detail("deadbeef").is_err());
    }

    #[test]
    fn file_view_keyed_on_path() {
        // A plain .go path -> a Diff; a *_test.go path -> a Source; non-go -> None.
        let diff = FixtureBackend.file_view("c", "app.go").unwrap();
        assert!(matches!(diff, Some(FileView::Diff(_))), "go file -> diff");
        let src = FixtureBackend
            .file_view("c", "nfs_flusher_test.go")
            .unwrap();
        assert!(matches!(src, Some(FileView::Source(_))), "_test.go -> source");
        let none = FixtureBackend.file_view("c", "README.md").unwrap();
        assert!(none.is_none(), "non-go -> no preview");
    }
}
