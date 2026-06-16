//! Integration gate: drive each layer seam through ONLY the public surface.
//!
//! Importing exclusively the narrow public API (the backend trait + carriers +
//! builder, plus `model`/`view_state`) means a widening of visibility OR a new
//! cross-layer call surfaces here as a compile or assertion break under
//! `cargo test`. `FixtureBackend` is crate-private by design, so this external
//! crate implements its OWN tiny `GitBackend` - proving the trait is usable from
//! outside the crate. One assertion per tier: backend -> model -> render-shape.

use gitgit::backend::{build_repo_model, BackendError, GitBackend, RepoSnapshot};
use gitgit::diff::FileView;
use gitgit::model::{self, Commit, CommitDetail, RepoModel, Signature};
use gitgit::view_state::ViewState;

/// A minimal external backend: one commit authored by the current user, no tree.
struct StubBackend;

impl GitBackend for StubBackend {
    fn load_repo(&self) -> Result<RepoSnapshot, BackendError> {
        Ok(RepoSnapshot {
            commits: vec![Commit {
                hash: "deadbeef".to_string(),
                full_hash: "deadbeef".to_string(),
                parents: vec![],
                subject: vec![],
                refs: vec![],
                author: "Me".to_string(),
                date: "01.01.2026, 00:00".to_string(),
                date_label: "01.01.2026, 00:00".to_string(),
                is_me: false,
                head: false,
                containing_branches: vec![],
                is_working: false,
                working: None,
            }],
            tree: vec![],
            default_selection: 0,
            current_user: "Me".to_string(),
            unpushed: Default::default(),
            has_remotes: false,
            truncated: false,
            status_sig: 0,
        })
    }

    fn commit_detail(&self, hash: &str) -> Result<CommitDetail, BackendError> {
        let sig = Signature {
            name: "Me".to_string(),
            email: String::new(),
            when: "01.01.2026, 00:00".to_string(),
        };
        Ok(CommitDetail {
            subject: String::new(),
            short_hash: hash.to_string(),
            author: sig.clone(),
            committer: sig,
            containing_branches: vec![],
            working: None,
        })
    }

    fn file_view(&self, _commit: &str, _path: &str) -> Result<Option<FileView>, BackendError> {
        Ok(None)
    }
}

/// Tier 1 (backend seam): a snapshot loads and carries the explicit current user;
/// BackendError is a public, Display-able error.
#[test]
fn backend_loads_snapshot() {
    let snap: RepoSnapshot = StubBackend.load_repo().expect("load_repo");
    assert_eq!(snap.commits.len(), 1);
    assert_eq!(snap.current_user, "Me");
    assert_eq!(BackendError("boom".to_string()).to_string(), "boom");
}

/// Tier 2 (model seam): build_repo_model yields one graph row per commit, empty
/// detail/preview, and stamps is_me from the snapshot's current_user.
#[test]
fn build_model_derives_graph_and_is_me() {
    let snap = StubBackend.load_repo().unwrap();
    let repo: RepoModel = build_repo_model(snap);
    assert_eq!(repo.graph.rows.len(), 1, "one graph row per commit");
    assert!(repo.detail.is_none() && repo.preview.is_none());
    assert!(repo.commits[0].is_me, "author == current_user -> is_me");
}

/// Tier 3 (view seam): the filtered-view model query runs over an empty repo
/// without panicking and yields no visible commits.
#[test]
fn view_query_over_empty_repo() {
    let repo = RepoModel::empty();
    let view = ViewState::new(0);
    assert_eq!(model::visible_commits(&repo, &view).len(), 0);
}
