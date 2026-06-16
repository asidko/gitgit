//! Layering enforcement: grep the source tree for forbidden cross-layer calls and
//! IO in the pure layers, asserting each search is EMPTY. Run by `cargo test`, so
//! a layering regression (an `apply` reaching into `ui`, IO in `store`, a render
//! dep in `backend`, ...) breaks the build instead of silently rotting.
//!
//! These are `-E` (extended regex) greps over paths under the crate source root.
//! Doc comments are authored to avoid the literal forbidden tokens so a match
//! always means real code, never prose.

use std::path::PathBuf;
use std::process::Command;

/// Crate source root (`<manifest>/src`).
fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Run `grep -rnE <pattern> <path>` and return matching lines (empty = no match).
/// grep exit code 1 (no match) is the success case; any other failure panics.
fn grep(pattern: &str, path: &str) -> String {
    let full = src_dir().join(path);
    let out = Command::new("grep")
        .arg("-rnE")
        .arg(pattern)
        .arg(&full)
        .output()
        .expect("run grep");
    match out.status.code() {
        Some(0) | Some(1) => String::from_utf8_lossy(&out.stdout).into_owned(),
        other => panic!("grep failed ({other:?}) for {pattern} in {}", full.display()),
    }
}

/// Assert a forbidden pattern does not appear under `path`.
fn assert_clean(pattern: &str, path: &str, why: &str) {
    let hits = grep(pattern, path);
    assert!(
        hits.trim().is_empty(),
        "layering violation ({why}): `{pattern}` found under src/{path}:\n{hits}"
    );
}

#[test]
fn ui_does_not_reach_into_other_layers() {
    // ui is PURE: it imports only model/view_state/theme/graph_engine/diff/highlight.
    assert_clean(
        r"crate::(store|message|backend|loader|data|tokenize)",
        "ui",
        "ui must not depend on store/message/backend/loader/data/tokenize",
    );
}

#[test]
fn store_does_not_reach_into_ui_or_backend() {
    // apply is the state seam: no ui (layering inversion), no backend (the
    // composition root injects data), no removed data module.
    assert_clean(
        r"crate::(ui|backend|loader|data)",
        "store.rs",
        "store must not call ui/backend/loader/data",
    );
}

#[test]
fn pure_layers_do_no_io() {
    // store/model/view_state are ZERO-IO and time-free (the loader WF owns threads).
    let io = r"(std::fs|std::process|std::thread|std::time|Instant|Command|::spawn)";
    for f in ["store.rs", "model.rs", "view_state.rs"] {
        assert_clean(io, f, "pure layer must not touch fs/process/thread/time");
    }
}

#[test]
fn backend_has_no_render_or_terminal_deps() {
    // backend is the git boundary: owned domain types only, no render/terminal.
    assert_clean(
        r"(ratatui|crossterm)",
        "backend",
        "backend must not depend on the render/terminal crates",
    );
}

#[test]
fn data_module_is_gone() {
    // The old fixture module was deleted; nothing references it.
    assert_clean(r"crate::data", "", "the data module must be fully removed");
}
