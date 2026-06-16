//! Headless snapshot mode: render one frame to a [`TestBackend`] and dump the
//! cell grid (symbol + fg/bg) as JSON. Used by the verification tooling to
//! render the TUI to an image without a real terminal.

use std::fs;
use std::io;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::Terminal;

use crate::store::AppState;
use crate::ui;

/// Render one frame to JSON. `real` false (default) uses the SYNCHRONOUS fixture
/// bootstrap so existing tooling + the golden test stay byte-identical; `real`
/// loads the real repository at the current working directory synchronously
/// (`bootstrap_real`), so a single frame shows THIS repo's data deterministically.
pub fn run(width: u16, height: u16, out_path: &str, real: bool) -> io::Result<()> {
    let state = if real {
        // Only the `--real` path reads disk config; the fixture path stays config-
        // free so the golden render is byte-identical regardless of any gitgit.toml.
        let (config, _warning) = crate::config::Config::load();
        let path = config
            .repo_path()
            .map(Ok)
            .unwrap_or_else(std::env::current_dir)?;
        crate::bootstrap_real(&path, &config).map_err(|e| io::Error::other(e.to_string()))?
    } else {
        crate::bootstrap_fixture()
    };
    let json = render_state_json(&state, width, height)?;
    fs::write(out_path, &json)?;
    let kind = if real { "real" } else { "fixture" };
    eprintln!("wrote {width}x{height} {kind} snapshot to {out_path}");
    Ok(())
}

/// Render the default fixture frame at `width`x`height` and serialize the cell
/// grid to the snapshot JSON. Shared by the golden render test so the
/// byte-identity ratchet hashes exactly the fixture frame.
pub fn render_default_json(width: u16, height: u16) -> io::Result<String> {
    render_state_json(&crate::bootstrap_fixture(), width, height)
}

/// Render any composed `state` to the snapshot JSON. The ONE render+dump path the
/// fixture and `--real` snapshots share, so both go through the identical pipeline.
fn render_state_json(state: &AppState, width: u16, height: u16) -> io::Result<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height))
        .map_err(|e| io::Error::other(e.to_string()))?;
    terminal
        .draw(|frame| ui::view(frame, &state.repo, &state.view, &state.status))
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(dump_json(terminal.backend().buffer(), width, height))
}

fn dump_json(buffer: &Buffer, width: u16, height: u16) -> String {
    let mut s = String::with_capacity((width as usize) * (height as usize) * 24);
    s.push_str(&format!("{{\"w\":{width},\"h\":{height},\"cells\":["));
    for y in 0..height {
        if y > 0 {
            s.push(',');
        }
        s.push('[');
        for x in 0..width {
            if x > 0 {
                s.push(',');
            }
            let cell = buffer.cell((x, y)).expect("cell in bounds");
            s.push_str(&format!(
                "{{\"s\":{},\"f\":\"{}\",\"b\":\"{}\"}}",
                json_str(cell.symbol()),
                hex(cell.fg),
                hex(cell.bg),
            ));
        }
        s.push(']');
    }
    s.push_str("]}");
    s
}

/// Map a [`Color`] to a `#rrggbb` string, or `"reset"` for the terminal default.
fn hex(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Reset => "reset".to_string(),
        Color::Black => "#000000".to_string(),
        Color::White => "#ffffff".to_string(),
        other => format!("{other:?}"),
    }
}

/// JSON-encode a (usually single-grapheme) cell symbol.
fn json_str(sym: &str) -> String {
    let mut out = String::with_capacity(sym.len() + 2);
    out.push('"');
    for ch in sym.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// -- B3 seam2 tests: (RepoModel, ViewState) -> cells --------------------------
//
// These render through the real `ui::view` into a `TestBackend` buffer and assert
// on cell text/bg, plus the GOLDEN GATE: the default fixture frame's JSON sha256
// must equal the captured baseline so any cross-layer or render change that moves
// a byte breaks `cargo test`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Commit, FileStatus, RepoModel, Status, SubjectSpan, TreeNode,
    };
    use crate::theme::{Glyph, Theme};
    use crate::view_state::ViewState;
    use ratatui::buffer::Buffer;
    use sha2::{Digest, Sha256};

    /// sha256 of the default fixture frame's snapshot JSON at 240x62: the
    /// byte-identity ratchet. If it changes, the default frame moved a cell -
    /// intentional renders must update it deliberately.
    ///
    /// Re-baselined for ONE INTENTIONAL render change, verified in the dumped frame: a one-row
    /// lazygit-style HINT BAR now occupies the bottom row ("Commit: c | Pull: p | ..."), every
    /// pane sitting one row higher; no other cell changes.
    ///
    /// PRIOR re-baselines: 508ecf7 files-pane search field in the files toolbar; 7c9bea1
    /// removed the search `Cc` case toggle; 6fd8ca4 tags -> branch
    /// DIAMOND family (◇/◆) + ringed branch-tip node (◉/◎); 45a8d1b inline fold marker row
    /// (dropped `folded_top`); 8b41029a
    /// files "A vs B" header + focus button + `<current>` chip;
    /// 89464f1e log column auto-fit; f8d65127 locality ◆/◇ ref diamonds; 903bd8c8 author
    /// abbreviation + ⌕ lens + dropped Revert button; 8e9f3d3f local-branch ◇/◆ diamonds;
    /// d9980cf toolbar seam BG; d9980cf menu flush; 17be785 author column 10 + subject 40%
    /// floor; 6682307 bottom action bar; c26cf92 log column tints; 4fd83c02 "Editor/View"
    /// menu bar; 135870bc "Flat"; 23a0ef07 "All"; (bar removed) bottom action bar dropped,
    /// the row reclaimed by the log/detail panes; da7798e7 third "Git" menu-bar label;
    /// cb685742 repo-root files-pane row; (row removed) the synthetic repo-root files-pane
    /// row dropped - the tree renders at depth 0 (one less indent, no chrome row);
    /// b4775827 toolbar refresh button (Update Project) added right of the Date filter;
    /// b9375ad4 diff-header pin buttons removed (the pin feature was dropped);
    /// 2e0d5daa bottom hint bar (pin-removal baseline b9375ad4).
    const GOLDEN_SHA256: &str =
        "2e0d5daa838cc488105daaeee6489639198c7c1aae0aeea39905ae51c34bc049";

    /// Render `(repo, view)` through the real `ui::view` into an owned buffer.
    /// Status `Ready` so the per-panel loading placeholders never overlay the
    /// assertion targets (the byte-identity test renders the fixture, always Ready).
    fn render_to_buffer(repo: &RepoModel, view: &ViewState, w: u16, h: u16) -> Buffer {
        render_with_status(repo, view, w, h, &Status::Ready)
    }

    /// Render `(repo, view, status)` through `ui::view` into an owned buffer, so the
    /// loading/error placeholder tests can exercise the lifecycle-driven panels.
    fn render_with_status(
        repo: &RepoModel,
        view: &ViewState,
        w: u16,
        h: u16,
        status: &Status,
    ) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test backend");
        terminal
            .draw(|frame| ui::view(frame, repo, view, status))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    /// The whole buffer flattened to one searchable string.
    fn whole_text(buf: &Buffer) -> String {
        (0..buf.area.height).map(|y| row_text(buf, y)).collect()
    }

    /// Concatenate the symbols of buffer row `y` into a string for substring asserts.
    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).map_or("", |c| c.symbol()))
            .collect()
    }

    /// A minimal hand-built repo (NO fixtures): one commit + one added file.
    fn tiny_repo(subject: &str) -> RepoModel {
        let mut repo = RepoModel::empty();
        repo.commits = vec![Commit {
            hash: "abc12345".to_string(),
            full_hash: "abc12345".to_string(),
            parents: vec![],
            subject: vec![SubjectSpan::plain(subject)],
            refs: vec![],
            author: "Tester".to_string(),
            date: "01.01.2026, 00:00".to_string(),
            date_label: "01.01.2026, 00:00".to_string(),
            is_me: false,
            head: false,
            containing_branches: vec![],
            is_working: false,
            working: None,
        }];
        repo.graph = crate::graph_engine::build_layout(&repo.commits);
        repo.tree = vec![TreeNode::File {
            name: "new.go".to_string(),
            status: FileStatus::Added,
        }];
        repo
    }

    #[test]
    fn empty_repo_renders_without_panic() {
        let repo = RepoModel::empty();
        let view = ViewState::new(0);
        let buf = render_to_buffer(&repo, &view, 120, 40);
        // No commit rows, but the frame paints (background present, no panic).
        assert_eq!(buf.area.width, 120);
        assert_eq!(buf.area.height, 40);
    }

    #[test]
    fn loading_status_shows_log_placeholder() {
        // Non-blocking startup: empty repo + Loading -> the log pane shows the
        // history placeholder (mock, no git). Filled once RepoLoaded arrives.
        let repo = RepoModel::empty();
        let view = ViewState::new(0);
        let buf = render_with_status(&repo, &view, 120, 40, &Status::Loading);
        assert!(
            whole_text(&buf).contains("Loading history..."),
            "the log pane must show the loading placeholder while Loading + empty"
        );
    }

    #[test]
    fn error_status_shows_error_line() {
        // A backend failure (e.g. cwd is not a git repo) is non-fatal: the empty
        // log pane shows a visible error line and the app keeps running (mock).
        let repo = RepoModel::empty();
        let view = ViewState::new(0);
        let status = Status::Error("not a git repository".to_string());
        let buf = render_with_status(&repo, &view, 120, 40, &status);
        let text = whole_text(&buf);
        assert!(
            text.contains("Error:") && text.contains("not a git repository"),
            "an error status must render a visible error line"
        );
    }

    #[test]
    fn commit_subject_appears_in_a_log_row() {
        let repo = tiny_repo("UNIQUESUBJECTXYZ matters");
        let view = ViewState::new(0);
        let buf = render_to_buffer(&repo, &view, 160, 40);
        let whole: String = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
        assert!(
            whole.contains("UNIQUESUBJECTXYZ"),
            "the built commit subject must render in a log row"
        );
    }

    #[test]
    fn added_diff_line_uses_added_bg() {
        // Render the fixture frame; an Added diff line must paint the Added band bg.
        let buf = render_to_buffer(
            &crate::bootstrap_fixture().repo,
            &crate::bootstrap_fixture().view,
            240,
            62,
        );
        let added_bg = Theme::DIFF_ADD_BG;
        let found = (0..buf.area.height).any(|y| {
            (0..buf.area.width).any(|x| buf.cell((x, y)).map(|c| c.bg) == Some(added_bg))
        });
        assert!(found, "an Added diff line must use the Added background");
    }

    #[test]
    fn golden_default_frame_byte_identity() {
        let json = render_default_json(240, 62).expect("render default frame");
        let digest = Sha256::digest(json.as_bytes());
        let hex = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(
            hex, GOLDEN_SHA256,
            "default fixture frame must stay byte-identical"
        );
    }

    #[test]
    fn binary_preview_shows_a_notice_not_a_blank_body() {
        // A FileView::Binary must render its notice in the diff body (so a changed
        // binary reads clearly), not leave the body blank like a 0-line diff.
        use crate::diff::{BinaryFile, FileView};
        let mut repo = tiny_repo("subject");
        repo.preview = Some(FileView::Binary(BinaryFile {
            path: "data.bin".to_string(),
            note: "Binary file differs".to_string(),
        }));
        let mut view = ViewState::new(0);
        view.focus = crate::view_state::Pane::Files; // file selected -> preview shown
        let buf = render_to_buffer(&repo, &view, 120, 40);
        let text = whole_text(&buf);
        assert!(text.contains("Binary file differs"), "the binary notice is rendered");
        assert!(text.contains("data.bin"), "the binary file path is in the header");
    }

    #[test]
    fn empty_ready_repo_shows_no_commits_notice() {
        // A genuinely empty (loaded) repo - empty/unborn HEAD - must show an explicit
        // "No commits" placeholder, not a blank log pane that looks broken.
        let repo = RepoModel::empty();
        let view = ViewState::new(0);
        let buf = render_with_status(&repo, &view, 120, 40, &Status::Ready);
        assert!(
            whole_text(&buf).contains("No commits"),
            "an empty Ready repo shows the No commits placeholder"
        );
    }

    #[test]
    fn tiny_frames_render_without_panic() {
        // Regression: on a short/narrow terminal the layout collapses panes to
        // height/width 0 and parks them AT the buffer edge (y == frame height); the
        // separators and the files toolbar then derived a 1-row rect ONE PAST the
        // last valid row and ratatui panicked with "index outside of buffer". A
        // ZERO-area frame (h==0 or w==0, a degenerate window / SIGWINCH resize race)
        // also panicked the toggles-bar pills. Every size from 0x0 up through the
        // prior crash threshold must now draw a (degraded) frame and return cleanly,
        // with the diff viewer shown AND hidden, and under a Notice (which paints the
        // files-toolbar text) as well as Ready.
        let repo = multi_file_repo();
        let notice = Status::Notice("a notice that paints the files toolbar".into());
        for show_diff in [true, false] {
            for status in [&Status::Ready, &notice] {
                for h in 0u16..=14 {
                    for w in [0u16, 1, 2, 5, 20, 52, 80, 126, 200] {
                        let mut view = ViewState::new(0);
                        view.show_diff = show_diff;
                        // Renders into a TestBackend; a write outside the buffer would
                        // panic here (the bug), so reaching the size assert proves the fix.
                        let buf = render_with_status(&repo, &view, w, h, status);
                        assert_eq!((buf.area.width, buf.area.height), (w, h));
                    }
                }
            }
        }
    }

    /// A repo with three sibling changed files, so the files pane has marked-able
    /// file rows for the multi-select render tests.
    fn multi_file_repo() -> RepoModel {
        let mut repo = tiny_repo("subject");
        repo.tree = vec![
            TreeNode::File { name: "alpha.go".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "beta.go".to_string(), status: FileStatus::Added },
            TreeNode::File { name: "gamma.go".to_string(), status: FileStatus::Deleted },
        ];
        repo
    }

    #[test]
    fn empty_marked_set_render_is_unchanged_for_the_files_pane() {
        // With NO marks, no cell in the files pane carries the marked band - the
        // byte-identity guarantee the golden gate enforces for the whole fixture
        // frame, asserted here at the pane level over a hand-built tree.
        let repo = multi_file_repo();
        let view = ViewState::new(0);
        let buf = render_to_buffer(&repo, &view, 160, 40);
        let marked = (0..buf.area.height)
            .any(|y| (0..buf.area.width).any(|x| buf.cell((x, y)).map(|c| c.bg) == Some(Theme::FILES_MARKED_BG)));
        assert!(!marked, "an empty marked set paints no marked band");
    }

    #[test]
    fn marked_file_row_paints_the_marked_band() {
        // Mark beta.go: at least one cell on its row carries the marked band bg, and
        // the cursor (on a DIFFERENT row) keeps the focus highlight - the two read
        // as distinct affordances.
        let mut view = ViewState::new(0);
        view.focus = crate::view_state::Pane::Files;
        view.files_sel = 0; // cursor on alpha.go
        view.files_marked.insert("beta.go".to_string());
        let repo = multi_file_repo();
        let buf = render_to_buffer(&repo, &view, 160, 40);
        let marked_present = (0..buf.area.height)
            .any(|y| (0..buf.area.width).any(|x| buf.cell((x, y)).map(|c| c.bg) == Some(Theme::FILES_MARKED_BG)));
        assert!(marked_present, "a marked file row shows the marked band");
        // The marked row also shows the filled gutter dot in the leading column.
        let gutter_dot = (0..buf.area.height).any(|y| {
            (0..buf.area.width).any(|x| {
                buf.cell((x, y)).map(|c| c.symbol()) == Some(&Glyph::MARK.to_string())
                    && buf.cell((x, y)).map(|c| c.bg) == Some(Theme::FILES_MARKED_BG)
            })
        });
        assert!(gutter_dot, "a marked file row shows the gutter mark glyph");
        // The cursor row still carries the focus highlight, distinct from the band.
        let cursor_present = (0..buf.area.height)
            .any(|y| (0..buf.area.width).any(|x| buf.cell((x, y)).map(|c| c.bg) == Some(Theme::SELECTION_FOCUS)));
        assert!(cursor_present, "the cursor keeps its focus highlight alongside marks");
    }

    #[test]
    fn files_toolbar_is_flat_all_then_focus() {
        use crate::ui::layout::{compute_layout, FilesAction};

        // The files toolbar holds Flat, All, then the Focus (bullseye) button, left to
        // right on one row. (Revert moved to the Editor menu - no longer a toolbar button.)
        let state = crate::bootstrap_fixture();
        let buf = render_to_buffer(&state.repo, &state.view, 240, 62);
        let lm = compute_layout(buf.area, &state.repo, &state.view);

        let actions: Vec<FilesAction> = lm.files_actions.iter().map(|c| c.action).collect();
        assert_eq!(
            actions,
            vec![FilesAction::Flat, FilesAction::AllFiles, FilesAction::Focus],
            "the files toolbar is Flat, All, then Focus"
        );
        let flat = lm.files_actions[0].rect;
        let all = lm.files_actions[1].rect;
        let focus = lm.files_actions[2].rect;
        assert_eq!(flat.y, all.y, "the controls share the toolbar row");
        assert_eq!(all.y, focus.y, "Focus shares the toolbar row");
        assert!(flat.x < all.x && all.x < focus.x, "Flat, then All, then Focus");
    }

    /// The fg color of the first cell whose symbol is `ch` WITHIN the files-list
    /// rect, so a file-name char is read back from the files pane (not, say, a
    /// matching letter in the diff or log). `None` if absent in that region.
    fn fg_of_char_in(buf: &Buffer, list: ratatui::layout::Rect, ch: char) -> Option<Color> {
        let want = ch.to_string();
        (list.y..list.bottom()).find_map(|y| {
            (list.x..list.right())
                .find(|&x| buf.cell((x, y)).map(|c| c.symbol()) == Some(&want))
                .and_then(|x| buf.cell((x, y)).map(|c| c.fg))
        })
    }

    #[test]
    fn unchanged_file_renders_plain_changed_file_keeps_its_color() {
        use crate::ui::layout::compute_layout;
        // The All-files view shows changed files in their status color and untouched
        // files in the plain default text color (no status accent). Distinct leading
        // chars (read inside the files-list rect) identify each name's color.
        let mut repo = tiny_repo("subject");
        repo.tree = vec![
            TreeNode::File { name: "qmodded.go".to_string(), status: FileStatus::Modified },
            TreeNode::File { name: "zplainfile.go".to_string(), status: FileStatus::Unchanged },
        ];
        let view = ViewState::new(0);
        let buf = render_to_buffer(&repo, &view, 160, 40);
        let list = compute_layout(buf.area, &repo, &view).files_list;

        // The Modified file keeps the blue LINK accent.
        assert_eq!(
            fg_of_char_in(&buf, list, 'q'),
            Some(Theme::LINK),
            "a Modified file name renders in the status (LINK) color"
        );
        // The Unchanged file renders in the plain TEXT color (NO status accent).
        let plain = fg_of_char_in(&buf, list, 'z').expect("the unchanged file name renders");
        assert_eq!(plain, Theme::TEXT, "an Unchanged file name renders in the plain text color");
        assert_ne!(plain, Theme::LINK, "Unchanged is NOT the Modified color");
        assert_ne!(plain, Theme::ACCENT_RUN, "Unchanged is NOT the Added color");
        assert_ne!(plain, Theme::ACCENT_CLOSE, "Unchanged is NOT the Deleted color");
    }

    #[test]
    fn revert_modal_open_renders_yes_no_box() {
        // With the revert modal open, the confirmation box + its Yes/No buttons render.
        let mut view = ViewState::new(0);
        view.revert_confirm = Some(crate::view_state::RevertRequest {
            commit_hash: "abc12345".to_string(),
            commit_label: "the subject".to_string(),
            paths: vec!["alpha.go".to_string(), "beta.go".to_string()],
        });
        let repo = multi_file_repo();
        let buf = render_to_buffer(&repo, &view, 160, 40);
        let text = whole_text(&buf);
        assert!(text.contains("Revert Selected Changes"), "modal title renders");
        assert!(text.contains("[Yes]") && text.contains("[No]"), "Yes/No buttons render");
        assert!(text.contains("alpha.go") && text.contains("beta.go"), "the target paths are listed");
    }
}
