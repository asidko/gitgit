//! gitgit library crate.
//!
//! The binary (`src/main.rs`) is a thin runtime shell over this library; this is
//! where the layered pipeline lives so integration tests in `tests/` can drive
//! each seam through the public surface. The strict layering is:
//!
//!   git  ->  app state (data)  ->  ui
//!
//! behind a swappable [`backend::GitBackend`] trait. The backend yields owned
//! domain types; `store::apply` mutates state with ZERO IO; `ui` renders purely.
//!
//! Public surface deliberately narrow: the backend trait + its data carriers +
//! `build_repo_model`, plus `model`/`view_state` for tests. The concrete backend
//! impls and the runtime keymap stay crate-internal.

pub mod backend;
pub mod config;
pub mod diff;
pub mod graph_engine;
pub mod highlight;
pub mod loader;
pub mod message;
pub mod model;
pub mod snapshot;
pub mod store;
pub mod textdiff;
pub mod theme;
pub mod tokenize;
pub mod ui;
pub mod view_state;

use std::path::Path;
use std::sync::Arc;

use backend::{build_repo_model, BackendError, FixtureBackend, GitBackend, RealBackend};
use config::Config;
use model::{RepoModel, Status};
use store::AppState;
use view_state::ViewState;

/// Composition root: wire the fixture backend into a fully-seeded [`AppState`],
/// SYNCHRONOUSLY, so the default frame is byte-identical to the pre-arch render.
///
/// This is the ONE place the backend, the model builder, and the store meet -
/// neither `store` nor `backend` depends on the other, so the wiring lives here
/// at the top. It loads the snapshot, builds the model, preselects the snapshot's
/// default file, and resolves that file's preview up front (the single synchronous
/// `file_view` call). The real runtime instead uses [`bootstrap_loading`] +
/// [`open_real_backend`] + the off-thread loader; this synchronous path drives the
/// default-frame snapshot and the golden byte-identity test.
pub fn bootstrap_fixture() -> AppState {
    let backend = FixtureBackend;
    let snapshot = backend.load_repo().expect("fixture backend never fails");
    let default_selection = snapshot.default_selection;
    let mut state = AppState::from_repo(build_repo_model(snapshot));
    // The fixture has no on-disk path; seed a deterministic repo-root label so the zip-archive
    // prefill is reproducible (the real boots derive it from the opened path in from_config).
    state.view.repo_root_name = "gitgit".to_string();
    // `default_selection` is the raw tree-flatten index, which now maps 1:1 to the visible row.
    state.preselect_file(default_selection);
    // Resolve the startup preview synchronously for the default (commit, path).
    let preview = match (state.selected_commit_hash(), state.selected_file_path()) {
        (Some(commit), Some(path)) => backend
            .file_view(&commit, &path)
            .expect("fixture file_view never fails"),
        _ => None,
    };
    state.set_preview(preview);
    state
}

/// Open the on-disk repository at `path` behind the [`GitBackend`] trait object the
/// loader holds, applying `config`'s `[behavior]` knobs (caps + date format). The
/// ONLY place the binary can reach the crate-private [`RealBackend`]; it returns an
/// `Arc<dyn GitBackend>` so `loader::spawn_loader` can own it across the worker
/// thread. A bad path (not a repo) errors here.
pub fn open_real_backend(
    path: &Path,
    config: &Config,
) -> Result<Arc<dyn GitBackend + Send + Sync>, BackendError> {
    Ok(Arc::new(RealBackend::open(path, config)?))
}

/// The non-blocking startup state: an EMPTY repo marked `Loading`, so the full
/// shell renders instantly while the loader thread fetches the real history. The
/// first [`message::Msg::RepoLoaded`] flips it to `Ready` and fills the panels.
/// `config`'s `[layout]`/`[columns]`/`[view]` seed the initial view (clamped); the
/// later `RepoLoaded` only swaps the repo data, never the user's view settings.
pub fn bootstrap_loading(config: &Config) -> AppState {
    let mut state = AppState::from_repo(RepoModel::empty());
    state.view = ViewState::from_config(config, state.view.files_sel);
    state.set_status(Status::Loading);
    state
}

/// Compose a FULLY-resolved [`AppState`] from the real repository at `path`,
/// SYNCHRONOUSLY (no thread): apply `config`'s behavior knobs + initial view, load
/// the repo, seed the default selection, and resolve that selection's detail, tree,
/// and highlighted preview up front. Used by the `--real` snapshot so one rendered
/// frame shows real data deterministically. Mirrors the loader's per-selection
/// upgrades, but inline, on the calling thread.
pub fn bootstrap_real(path: &Path, config: &Config) -> Result<AppState, BackendError> {
    let backend = RealBackend::open(path, config)?;
    let snapshot = backend.load_repo()?;
    let default_selection = snapshot.default_selection;
    let mut state = AppState::from_repo(build_repo_model(snapshot));
    state.view = ViewState::from_config(config, state.view.files_sel);
    state.preselect_file(default_selection);
    if let Some(hash) = state.selected_commit_hash() {
        state.apply(message::Msg::DetailLoaded {
            detail: backend.commit_detail(&hash)?,
            hash: hash.clone(),
        });
        let (tree, ignored) = if state.view.show_all_files {
            (backend.full_tree(&hash)?, backend.ignored_paths(&hash)?)
        } else {
            (backend.changed_files(&hash)?, std::collections::HashSet::new())
        };
        state.apply(message::Msg::TreeLoaded { tree, ignored, hash });
    }
    if let (Some(commit), Some(path)) =
        (state.selected_commit_hash(), state.selected_file_path())
    {
        let view = backend.file_view(&commit, &path)?.map(tokenize::highlight);
        state.apply(message::Msg::PreviewLoaded { commit, path, view });
    }
    Ok(state)
}
