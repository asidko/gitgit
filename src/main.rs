//! gitgit - terminal git client (ratatui).
//!
//! The binary is a thin runtime shell over the `gitgit` library: it owns the
//! event loop, the keymap/mousemap (physical input -> `Msg` intents), and the
//! transient drag state, then delegates all model + render work to the library.
//! Run with no args for the interactive TUI, or `gitgit snapshot <w> <h>
//! <out.json>` to dump one rendered frame for testing.

use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::crossterm::ExecutableCommand;
use ratatui::layout::Rect;
use ratatui::Terminal;

use gitgit::config::Config;
use gitgit::loader::{spawn_loader, Req};
use gitgit::message::{Dir, EditOp, Msg};
use gitgit::store::AppState;
use gitgit::view_state::Effect;
use gitgit::ui::layout::{
    compute_layout, row_to_index, Divider, FilterKind, HandleKind, LayoutMap, SplitAxis,
};
use gitgit::{snapshot, ui, view_state};

/// How long the loop waits for input before draining the backend channel.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// `--watch` repo-reload poll period.
const WATCH_PERIOD: Duration = Duration::from_secs(5);

/// Always-on working-tree change-detection poll period: the worker recomputes the
/// (HEAD + statuses) signature and the repo refreshes only when it actually changed,
/// so the files panel tracks external edits without `--watch`'s unconditional reloads.
const STATUS_POLL_PERIOD: Duration = Duration::from_secs(2);

/// Buffer rows the caret moves per mouse-wheel notch over the editable diff (a
/// conventional 3-line wheel step); the cursor-derived scroll follows it.
const WHEEL_LINES: isize = 3;

/// Code columns the diff scrolls sideways per horizontal-wheel notch (shift+wheel or a
/// native horizontal wheel), word-wrap off. A wider step than the vertical line step so
/// reaching a long line's tail does not take many notches.
const HSCROLL_COLS: isize = 8;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", help_text());
        return Ok(());
    }
    if args.get(1).map(String::as_str) == Some("snapshot") {
        let w = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200);
        let h = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(50);
        let out = args.get(4).map(String::as_str).unwrap_or("snapshot.json");
        let real = args.iter().any(|a| a == "--real");
        return snapshot::run(w, h, out, real);
    }
    // `--watch` periodically reloads the repo so the panes track on-disk changes.
    let watch = args.iter().any(|a| a == "--watch").then_some(WATCH_PERIOD);
    run_tui(watch)
}

/// Canonical `-h` / `--help` text: usage, flags, key bindings.
fn help_text() -> String {
    "gitgit - terminal git client\n\
         \n\
         USAGE:\n    \
             gitgit [OPTIONS]\n    \
             gitgit snapshot <W> <H> [OUT.json] [--real]\n\
         \n\
         OPTIONS:\n    \
             --watch           Periodically reload the repo so panes track on-disk\n                      \
                         changes (every 5s).\n    \
             --real           (snapshot only) render the real cwd repo, not the fixture.\n    \
             -h, --help       Show this help and exit.\n\
         \n\
         CONFIG:\n    \
             Reads gitgit.toml from the cwd (see gitgit.example.toml). [repo].path sets\n    \
             the repository; defaults to the current directory.\n\
         \n\
         MENUS (top bar):\n    \
             Editor   Undo / Redo / Autosave / Revert\n    \
             View     Show Diff / Side by side / Word wrap / Whitespace\n\
         \n\
         EDITING: the diff's RIGHT side is your live working file - just focus it\n    \
             (Tab or click) and type. The left side is the selected commit; the diff\n    \
             updates as you type, in side-by-side or unified. Click places the cursor,\n    \
             drag or Shift+arrows select, Ctrl+C/X/V copy/cut/paste, Ctrl+Z/Ctrl+Y\n    \
             undo/redo, Ctrl+S saves, Esc leaves (autosaves). Moving to another file\n    \
             autosaves too.\n\
         \n\
         KEYS:\n    \
             arrows/jk  move    Tab  cycle Log/Files/Diff    Enter  open\n    \
             Space      mark file        Alt+R  revert marked file(s)\n    \
             Alt+A      all-files view   Alt+D  toggle diff\n    \
             F10 / q / Ctrl+Q quit   (Esc only leaves editing / closes popups)\n"
        .to_string()
}

/// Set up the alternate screen, run the event loop, and always restore.
///
/// Config is read ONCE here, at boot (sub-ms, does NOT block the first frame): it
/// reads disk for `gitgit.toml`. A malformed config is NON-FATAL - its warning is
/// carried into the first `Status::Error` line.
fn run_tui(watch: Option<Duration>) -> io::Result<()> {
    let (config, warning) = Config::load();

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    // `[behavior].mouse` gates mouse capture (drag-resize / wheel / click-select).
    if config.behavior.mouse {
        io::stdout().execute(EnableMouseCapture)?;
    }
    io::stdout().execute(Hide)?;

    // Restore the terminal even if a panic unwinds through the draw loop.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        original_hook(info);
    }));

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = event_loop(&mut terminal, &config, warning, watch);

    restore()?;
    result
}

/// Tear down terminal modes set up by [`run_tui`]. Also runs from the panic hook.
fn restore() -> io::Result<()> {
    disable_raw_mode()?;
    io::stdout()
        .execute(DisableMouseCapture)?
        .execute(LeaveAlternateScreen)?
        .execute(Show)?;
    Ok(())
}

/// The persisted View/Editor/files toggles as a comparable signature. The run loop saves
/// `state.toml` the instant any of these flips (not only on a clean quit) so a toggle
/// survives an abrupt exit - a terminal close (SIGHUP), kill, or crash never reaches the
/// quit-time save. Search-history length is folded in so a committed query persists too.
fn toggle_sig(v: &view_state::ViewState) -> (bool, bool, bool, bool, bool, bool, bool, bool, bool, usize) {
    (
        v.diff_mode == view_state::DiffMode::SideBySide,
        v.word_wrap,
        v.show_whitespace,
        v.show_diff,
        v.hide_unchanged,
        v.autosave,
        v.files_flat,
        v.show_all_files,
        v.show_blame,
        v.search_history.len(),
    )
}

/// Poll loop: redraw only when dirty, map keys to intents at this boundary, and
/// drain backend pushes from the channel. `dirty` is OR'd from every applied
/// message (input or backend), so any state change triggers exactly one redraw.
///
/// A single loader thread owns git IO: the runtime starts `Loading` over an empty
/// repo, the loader pushes `RepoLoaded`, and the runtime then drives per-selection
/// `Req`s.
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    config: &Config,
    warning: Option<String>,
    watch: Option<Duration>,
) -> io::Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let (mut state, mut loader) = boot(config, warning, tx);
    // Overlay the user's remembered pane splits + search history over the config
    // defaults, so a prior drag-resize is honored across restarts.
    if let Some(st) = gitgit::config::load_layout_state() {
        state.view.overlay_layout_state(&st);
    }
    let mut dirty = true;
    // Last drawn frame size, cached so mouse hit-testing recomputes the same
    // layout the draw used without coupling `ui` to the runtime.
    let mut last_size = Rect::default();
    // Transient drag interaction: the handle the left button is holding (a pane
    // separator or a log column boundary), if any. Pure runtime UI state (never
    // model/apply), mirroring the keymap rule.
    let mut drag: Option<HandleKind> = None;
    // Multi-click tracker for the editable diff: a second/third left-press on the same
    // cell within the window selects the word / line (double / triple click).
    let mut clicks = ClickTracker::default();
    // True only while a left-drag that STARTED on the editable new side is in flight, so
    // a drag begun in the log/files pane and dragged into the diff cannot steal focus or
    // start a phantom text selection.
    let mut edit_dragging = false;
    // True only while a left-drag that STARTED in the commit-detail pane is in flight,
    // so a drag begun elsewhere cannot extend the detail text selection.
    let mut detail_dragging = false;
    // True only while a left-drag that STARTED on a read-only diff body is in flight, so
    // a drag begun elsewhere cannot extend the diff text selection.
    let mut diff_dragging = false;
    // True only while a left-drag that STARTED on the diff's horizontal scrollbar thumb is
    // in flight, so the drag scrolls sideways regardless of vertical drift.
    let mut hbar_dragging = false;
    // The grab point WITHIN the scrollbar thumb at press, so a drag keeps the thumb under
    // the pointer (a re-click on the thumb must not jump).
    let mut hbar_grab: u16 = 0;
    // A held edit-drag pinned at the pane edge: the run loop re-emits this Move each idle
    // tick (terminals stop streaming Drag events for a stationary pointer) so a selection
    // keeps growing + scrolling off the edge.
    let mut edge_scroll: Option<Dir> = None;
    // `--watch`: fire a `Req::Reload` every `period`. Coalesced by the loader, so a
    // slow load can never stack ticks. No-op when watch is disabled.
    let mut next_watch = watch.map(|p| Instant::now() + p);
    // Always-on external-change detection: a cheap signature poll every tick; the
    // store requests a refresh only when the signature moved (Msg::StatusPolled).
    let mut next_status_poll = Instant::now() + STATUS_POLL_PERIOD;
    // The last persisted toggle signature: when it changes we write state.toml at once, so
    // a flipped toggle is durable even if the process never reaches the quit-time save.
    let mut last_toggle_sig = toggle_sig(&state.view);

    while !state.quit {
        if let (Some(period), Some(at)) = (watch, next_watch) {
            if Instant::now() >= at {
                // Editor-lock: never reload while an editable buffer is OPEN (dirty or
                // clean) - a reload swaps the repo under the buffer and can shift the
                // selection beneath the caret. `RepoLoaded` is editor-aware too (the
                // belt for an already-in-flight reload); this avoids it entirely.
                if state.view.editor.is_none() {
                    loader.request(Req::Reload);
                }
                next_watch = Some(Instant::now() + period);
            }
        }
        if Instant::now() >= next_status_poll {
            loader.request(Req::StatusPoll);
            next_status_poll = Instant::now() + STATUS_POLL_PERIOD;
        }

        if dirty {
            terminal.draw(|frame| {
                last_size = frame.area();
                ui::view(frame, &state.repo, &state.view, &state.status);
            })?;
            dirty = false;
        }

        if event::poll(POLL_INTERVAL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let lm = compute_layout(last_size, &state.repo, &state.view);
                    // Ctrl+C with a live detail-pane selection copies it to the system
                    // clipboard (the runtime composes the text, since it needs the pane
                    // width); otherwise the key routes normally.
                    let msg = detail_copy_key(key.code, key.modifiers, &state, &lm)
                        .or_else(|| diff_copy_key(key.code, key.modifiers, &state))
                        .or_else(|| diff_hscroll_key(key.code, key.modifiers, &state, &lm))
                        .or_else(|| route_key(key.code, key.modifiers, &state.view, &lm));
                    if let Some(msg) = msg {
                        dirty |= apply_and_request(&mut state, msg, &mut loader);
                    }
                }
                Event::Mouse(mouse) => {
                    let lm = compute_layout(last_size, &state.repo, &state.view);
                    if let Some(msg) = map_mouse(
                        mouse,
                        &lm,
                        &state,
                        &mut drag,
                        &mut clicks,
                        &mut edit_dragging,
                        &mut detail_dragging,
                        &mut diff_dragging,
                        &mut hbar_dragging,
                        &mut hbar_grab,
                        &mut edge_scroll,
                    ) {
                        dirty |= apply_and_request(&mut state, msg, &mut loader);
                    }
                }
                // A terminal resize forces one redraw (see `forces_redraw`); the
                // in-progress drag is abandoned since its handle geometry referred to
                // the old size and is meaningless after the reflow.
                ev if forces_redraw(&ev) => {
                    drag = None;
                    dirty = true;
                }
                _ => {}
            }
        } else if edit_dragging {
            // Idle tick with a held edit-drag pinned at the pane edge: re-emit the Move so
            // the selection keeps growing + the diff keeps scrolling sideways.
            if let Some(dir) = edge_scroll {
                let msg = Msg::Edit(EditOp::Move { dir, select: true });
                dirty |= apply_and_request(&mut state, msg, &mut loader);
            }
        }

        dirty |= drain_backend(&rx, &mut state, &mut loader);

        // A toggle flip is durable immediately, not deferred to the quit-time save - so
        // closing the terminal (no clean quit) can't drop it. Splits/history still ride the
        // quit save (a drag would otherwise thrash the file); a toggle also re-saves them.
        let sig = toggle_sig(&state.view);
        if sig != last_toggle_sig {
            gitgit::config::save_layout_state(&state.view);
            last_toggle_sig = sig;
        }
    }
    // Remember the (possibly drag-adjusted) pane splits + search history for next run.
    gitgit::config::save_layout_state(&state.view);
    Ok(())
}

/// Compose the startup state + loader. Opens the `[repo].path` (or cwd) repository
/// behind the loader thread (kicking the initial `load_repo`); a non-repo path is
/// non-fatal - it pushes `BackendError` so `Status::Error` renders while the app
/// keeps running on a no-op loader. A malformed-config `warning` is surfaced LAST
/// (also non-fatal) so the user sees it even when the repo loads. The ONE composition
/// point for the runtime; `config` is read-only from here on.
fn boot(config: &Config, warning: Option<String>, tx: Sender<Msg>) -> (AppState, Loader) {
    let mut state = gitgit::bootstrap_loading(config);
    // Seed today's date here (the runtime owns the clock; `apply` stays clock-free for the
    // golden's determinism) for the `<current>` zip-archive prefill's date suffix.
    state.view.today = gitgit::backend::build::today_iso();
    let path = config
        .repo_path()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let (state, loader) = match gitgit::open_real_backend(&path, config) {
        Ok(backend) => (state, Loader::real(spawn_loader(backend, tx.clone()))),
        Err(_) => {
            // Non-fatal: surface the error through the same channel apply handles.
            tx.send(Msg::BackendError("not a git repository".to_string())).ok();
            (state, Loader::noop())
        }
    };
    // A malformed-config warning is non-fatal: shown after any repo error so it is
    // the visible `Status::Error` line while the app still loads the repository.
    if let Some(w) = warning {
        tx.send(Msg::BackendError(w)).ok();
    }
    (state, loader)
}

/// The runtime's side of the loader: the `Req` channel (absent when the repo failed
/// to open) plus the previously-loaded selection, so selection -> lazy-load requests
/// are decided HERE (never in `apply`). Tracking prev selection in the runtime keeps
/// `apply` ZERO-IO; the loader thread is the SOLE IO owner.
struct Loader {
    req_tx: Option<Sender<Req>>,
    prev_commit: Option<String>,
    prev_file: Option<String>,
    /// The files-pane mode (`show_all_files`) the current tree was last requested
    /// in. A flip of the All toggle re-requests the SAME commit's tree in the new
    /// mode (changed vs full), so the toggle reloads even without a commit move.
    prev_all_files: bool,
    /// One-shot: re-request the selected commit's tree even though neither the
    /// commit nor the All mode moved (set by `Effect::ReloadTree`).
    force_tree: bool,
    /// One-shot: re-request the selected commit's full detail (set by
    /// `Effect::ReloadDetail` after a selection-keeping repo refresh).
    force_detail: bool,
    /// One-shot: re-fetch the open file's blame gutter (set by `Effect::ReloadBlame`).
    force_blame: bool,
}

impl Loader {
    /// A live loader over its request channel.
    fn real(req_tx: Sender<Req>) -> Self {
        Loader { req_tx: Some(req_tx), ..Self::noop() }
    }

    /// A no-op loader (a failed real open with no usable repository): selection
    /// changes request nothing.
    fn noop() -> Self {
        Loader {
            req_tx: None,
            prev_commit: None,
            prev_file: None,
            prev_all_files: false,
            force_tree: false,
            force_detail: false,
            force_blame: false,
        }
    }

    /// Send a request to the worker (no-op without a channel: a failed-open repo).
    fn request(&self, req: Req) {
        if let Some(tx) = &self.req_tx {
            tx.send(req).ok();
        }
    }

    /// Drain the apply layer's queued [`Effect`]s in FIFO order: the one exhaustive
    /// consumer of everything `apply` (ZERO-IO) asks the runtime to do. Arrival order
    /// is preserved by construction (a save queued before a git write reaches the
    /// worker first); adding an `Effect` variant without an arm here is a compile
    /// error, so a new async op can never ship as a silent dead mailbox.
    fn drain_effects(&mut self, state: &mut AppState) {
        // The navigation epoch at queue time: stamped into the dialog-opening reads so
        // their replies can be dropped if the user navigates before they land.
        let epoch = state.view.nav_epoch;
        for effect in std::mem::take(&mut state.view.effects) {
            match effect {
                Effect::Revert(req) => {
                    self.request(Req::Revert { commit: req.commit_hash, paths: req.paths })
                }
                Effect::Save { path, content } => self.request(Req::SaveFile { path, content }),
                Effect::HunkRevert { commit, path, hunk } => {
                    self.request(Req::RevertHunk { commit, path, hunk })
                }
                Effect::Git(action) => self.request(Req::Git(action)),
                Effect::CopyPatch(file) => self.request(Req::CopyPatch { file }),
                Effect::CopyPatchMulti(paths) => self.request(Req::CopyPatchMulti { paths }),
                Effect::LoadMore => self.request(Req::LoadMore),
                Effect::Inspect(req) => self.request(Req::Inspect {
                    rev: req.rev,
                    path: req.path,
                    title: req.title,
                    mode: req.mode,
                    base: req.base,
                }),
                Effect::PickList(req) => self.request(Req::PickList {
                    kind: req.kind,
                    path: req.path,
                    mode: req.mode,
                    epoch,
                }),
                Effect::LoadRemotes => self.request(Req::Remotes { epoch }),
                Effect::RefPick(op) => self.request(Req::RefList { op, epoch }),
                Effect::ReloadRepo => {
                    // Reset the selection cache so the new tree/detail/preview all
                    // re-fire on the next `RepoLoaded` (a commit/amend can move HEAD).
                    self.prev_commit = None;
                    self.prev_file = None;
                    self.request(Req::Reload);
                }
                // The status poll's gentle refresh: same reload, selection cache KEPT,
                // so the open editor buffer / loaded file view are not re-fetched (the
                // store re-requests the tree + a read-only preview itself).
                Effect::RefreshRepo => self.request(Req::Reload),
                Effect::Clipboard(text) => copy_to_clipboard(&text),
                // Force a re-open even when the selection did not move (a hunk revert /
                // fold toggle changed what the same selection should show).
                Effect::ReloadPreview => self.prev_file = None,
                Effect::ReloadTree => self.force_tree = true,
                Effect::ReloadDetail => self.force_detail = true,
                Effect::ReloadBlame => self.force_blame = true,
            }
        }
    }

    /// After a state change, diff the new selection against the last-loaded one and
    /// request the lazy upgrades: a new commit -> Detail + Tree; a new file ->
    /// Preview. The detail/tree/preview staleness guards in `apply` drop any reply
    /// that lands after the selection moved on, so over-requesting is harmless.
    fn sync_selection(&mut self, state: &mut AppState) {
        let req_tx = match &self.req_tx {
            Some(tx) => tx,
            None => return,
        };
        let commit = state.selected_commit_hash();
        let all = state.view.show_all_files;
        // Reload the tree when the commit moves OR the All toggle flips: a new commit
        // gets Detail + its tree; a same-commit mode flip re-fetches only the tree in
        // the new mode (changed vs full). The detail is mode-independent, so it is
        // requested only on a commit move. A revert in the All view also queues a
        // ReloadTree effect (the reverted files still exist on disk, so they are not
        // pruned - the fresh full tree shows their new Unchanged/restored status).
        let commit_moved = commit != self.prev_commit;
        let mode_flipped = all != self.prev_all_files;
        let revert_reload = std::mem::take(&mut self.force_tree);
        let detail_reload = std::mem::take(&mut self.force_detail);
        if commit_moved || detail_reload {
            if let Some(hash) = &commit {
                req_tx.send(Req::Detail(hash.clone())).ok();
            }
        }
        if commit_moved || mode_flipped || revert_reload {
            if let Some(hash) = &commit {
                req_tx.send(Req::Tree { hash: hash.clone(), all }).ok();
            }
            self.prev_all_files = all;
        }
        if commit_moved {
            // A new commit's tree reloads, so the prior file no longer applies.
            self.prev_file = None;
            self.prev_commit = commit.clone();
        }
        let file = state.selected_file_path();
        let file_changed = file != self.prev_file;
        if file_changed {
            if let (Some(c), Some(path)) = (&commit, &file) {
                req_tx
                    .send(Req::OpenFile {
                        commit: c.clone(),
                        path: path.clone(),
                    })
                    .ok();
            }
            self.prev_file = file.clone();
        }
        // View > Blame: (re)fetch the per-line blame whenever the file changes OR the toggle
        // was just turned on, so the gutter follows the selection. The rev is the selected
        // commit (`WORKING_REV` blames the working tree on `<current>`).
        let blame_reload = std::mem::take(&mut self.force_blame);
        if state.view.show_blame && (file_changed || blame_reload) {
            if let (Some(c), Some(path)) = (&commit, &file) {
                req_tx.send(Req::Blame { rev: c.clone(), path: path.clone() }).ok();
            }
        }
    }
}

/// Apply `msg`, then drain the queued effects and request any lazy loads the new
/// selection needs. The selection -> `Req` wiring lives at this runtime boundary,
/// NOT in `apply` (which stays ZERO-IO). Returns whether a redraw is needed.
fn apply_and_request(state: &mut AppState, msg: Msg, loader: &mut Loader) -> bool {
    let dirty = state.apply(msg);
    loader.drain_effects(state);
    loader.sync_selection(state);
    dirty
}

/// Spawn a clipboard helper and feed it `text` on stdin. Tries Wayland then X11.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: [(&str, &[&str]); 2] = [
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
    ];
    for (cmd, args) in candidates {
        let Ok(mut child) = Command::new(cmd).args(args).stdin(Stdio::piped()).spawn() else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        // Check the EXIT STATUS, not just the spawn: `wl-copy` is often installed on an X11
        // session where it cannot reach a Wayland compositor and exits non-zero. Without this
        // check the loop would `return` after that failed first candidate and never try xclip,
        // so the copy silently vanished. Fall through to the next helper on any failure.
        if matches!(child.wait(), Ok(status) if status.success()) {
            return;
        }
    }
}

/// Apply every queued backend message; returns whether any redraw is needed. The
/// first `RepoLoaded` seeds the default selection's lazy loads (Detail+Tree+
/// Preview) via the same selection-diff every input change uses.
fn drain_backend(rx: &Receiver<Msg>, state: &mut AppState, loader: &mut Loader) -> bool {
    let mut dirty = false;
    while let Ok(msg) = rx.try_recv() {
        dirty |= apply_and_request(state, msg, loader);
    }
    dirty
}

/// Route a key according to the current input mode, then fall through to the
/// global keymap. The modal pre-branches keep keycodes at this boundary: while
/// the search field is focused, characters edit the query; while a filter
/// dropdown is open, the arrows/Enter/Esc drive the popup. Ctrl-modified keys
/// always fall through so Ctrl+C still quits from any mode.
///
/// `lm` lets the geometry-dependent keys share the click path's single decision:
/// the branch-list toggle (`B`) only fires when a link actually exists, exactly
/// as `left_click` gates on `lm.branches_link.is_some()`.
/// Ctrl+C while a non-empty detail-pane selection is live: compose the selected text
/// (needs the pane width, hence `lm` + `state`) and copy it to the system clipboard.
/// `None` lets Ctrl+C route normally (editor copy / quit) when nothing is selected.
fn detail_copy_key(
    code: KeyCode,
    mods: KeyModifiers,
    state: &AppState,
    lm: &LayoutMap,
) -> Option<Msg> {
    if !mods.contains(KeyModifiers::CONTROL) || code != KeyCode::Char('c') {
        return None;
    }
    // While a modal is open, Ctrl+C belongs to it, never to a detail-pane copy: the
    // input dialog uses Ctrl+C to copy its field (Ctrl+Q quits), and the revert modal
    // keeps Ctrl+C as its quit hatch. Either way, don't shadow it here.
    if state.view.dialog.is_some() || state.view.revert_confirm.is_some() {
        return None;
    }
    state.view.detail_sel?;
    let d = state.repo.detail.as_ref()?;
    let text = ui::detail_panel::selected_text(d, &state.view, lm.detail)?;
    Some(Msg::CopyText(text))
}

fn route_key(
    code: KeyCode,
    mods: KeyModifiers,
    view: &view_state::ViewState,
    lm: &LayoutMap,
) -> Option<Msg> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    // The revert confirmation modal is fully modal: while it is open, y/Enter
    // confirms, n/Esc cancels, and EVERY other key is swallowed (returns None) so
    // it cannot leak to the panes. Ctrl+C still quits (the universal escape hatch).
    if view.revert_confirm.is_some() {
        if ctrl && code == KeyCode::Char('c') {
            return Some(Msg::Quit);
        }
        return modal_key(code);
    }
    // An action-bar dialog (commit/amend/tag input, push/pull confirm, copy picker) is
    // fully modal: its keys drive the dialog and every other key is swallowed so it
    // cannot leak to the panes. Ctrl+Q quits (the universal escape hatch; Ctrl+C is now
    // the input dialog's copy, matching the editor). Esc cancels the dialog.
    if view.dialog.is_some() {
        if ctrl && code == KeyCode::Char('q') {
            return Some(Msg::Quit);
        }
        return dialog_key(code, mods, view);
    }
    // An open commit context menu is mouse-driven (click a row to pick); Esc closes
    // it. Checked early so it owns Esc while open (it overlays the panes). A first Esc
    // closes an open branch/tag fly-out (toggling it shut), a second closes the menu.
    if view.commit_menu.is_some() && code == KeyCode::Esc {
        if let Some(ref_idx) = view.commit_menu.as_ref().and_then(|m| m.open_ref) {
            return Some(Msg::OpenRefSubmenu { ref_idx });
        }
        return Some(Msg::CloseCommitMenu);
    }
    // The files-pane context menu is mouse-driven too; Esc closes it.
    if view.files_menu.is_some() && code == KeyCode::Esc {
        return Some(Msg::CloseFilesMenu);
    }
    // A read-only inspect overlay (Show Current Revision / a compared revision) owns the
    // keyboard while open: Esc closes it and every other key is SWALLOWED so nothing leaks to
    // the masked editor / the diff-cursor hunk-revert beneath it. The overlay hosts NO text
    // input, so the documented exit hatches (F10 / q / Ctrl+Q / Ctrl+C) still quit - matching
    // `map_key`. (Copy of a read-only selection + horizontal scroll keys resolve BEFORE
    // route_key, so they still work over the overlay.)
    if view.inspect.is_some() {
        if matches!(code, KeyCode::F(10) | KeyCode::Char('q'))
            || (ctrl && matches!(code, KeyCode::Char('q') | KeyCode::Char('c')))
        {
            return Some(Msg::Quit);
        }
        if code == KeyCode::Esc {
            return Some(Msg::CloseInspect);
        }
        return None;
    }
    // The lens's recent-search history popup is mouse-driven (click a row to run it);
    // Esc closes it. Checked before the search-field block so it owns Esc while open.
    if view.search_history_open && code == KeyCode::Esc {
        return Some(Msg::ToggleSearchHistory);
    }
    // A focused search field captures text input BEFORE the editor branch below: the
    // user can click the search field while the diff editor is also focused (without
    // leaving the editor), and the explicit query input must win - otherwise every
    // keystroke would edit the buffer and the search box would stay empty. Alt+mnemonic
    // commands still resolve first so accelerators keep working while searching.
    if view.search_active {
        if let Some(msg) = alt_accelerator(code, mods) {
            return Some(msg);
        }
        if !ctrl {
            if let Some(msg) = search_key(code) {
                return Some(msg);
            }
        }
    }
    // The files-pane search field captures text input the same way (it can be focused
    // while the editor is also focused), so its keys win before the editor branch.
    if view.files_search_active {
        if let Some(msg) = alt_accelerator(code, mods) {
            return Some(msg);
        }
        if !ctrl {
            if let Some(msg) = files_search_key(code) {
                return Some(msg);
            }
        }
    }
    // Alt+letter command accelerators (action bar + filters + toggles) resolve BEFORE
    // the editor branch, so they work even while the editable diff is focused (Alt
    // requires the modifier, so a bare letter still types normally into the buffer).
    // Without this the editor's catch-all swallows every Alt key.
    if let Some(msg) = alt_accelerator(code, mods) {
        return Some(msg);
    }
    // While the diff pane is focused on an EDITABLE file, the keyboard edits the
    // working buffer in place: text keys insert, Ctrl+S saves, clipboard keys cut/
    // copy/paste, Esc leaves editing. Every key is captured so none leaks to shortcuts
    // (e.g. `d` types a `d`, it does not toggle the diff). Ctrl+Q always quits.
    if view.focus == view_state::Pane::Diff && view.editor.is_some() {
        return editor_key(code, mods);
    }
    // (Alt+letter accelerators were already resolved above, before the editor branch.)
    if !ctrl {
        if view.search_active {
            if let Some(msg) = search_key(code) {
                return Some(msg);
            }
        } else if view.open_dropdown.is_some() {
            if let Some(msg) = dropdown_key(code, view.dropdown_sel) {
                return Some(msg);
            }
        } else if view.open_menu.is_some() && code == KeyCode::Esc {
            // An open menu is mouse-driven (items toggle on click); Esc closes it.
            return Some(Msg::CloseMenu);
        }
        // The branch-list toggle has no effect when there is no link to toggle
        // (names fit / single short branch). Gate it on the same geometry the
        // mouse click uses so key and click never disagree.
        if code == KeyCode::Char('B') {
            return lm.branches_link.is_some().then_some(Msg::ToggleBranchList);
        }
        if let Some(msg) = files_select_key(code, mods, view) {
            return Some(msg);
        }
        if let Some(msg) = log_select_key(code, mods, view) {
            return Some(msg);
        }
        // Enter on the focused diff pane reverts the hunk under the cursor (instead of
        // the files-tree expand/collapse Enter maps to elsewhere).
        if view.focus == view_state::Pane::Diff && code == KeyCode::Enter {
            return Some(Msg::RevertHunk);
        }
        // Esc leaves a focused diff pane (back to Files), for a READ-ONLY/browse diff
        // (no editable buffer - the editor path handles its own Esc above).
        if view.focus == view_state::Pane::Diff && code == KeyCode::Esc {
            return Some(Msg::DiffBlur);
        }
    }
    map_key(code, mods)
}

/// Files-pane multi-selection keys, active only while the Files pane is focused
/// (so the Log pane keeps its plain arrows). Space toggles the cursor row's mark;
/// Shift+Up/Down range-selects toward the neighbour; Esc clears the marks (only
/// when some exist, so a no-mark Esc falls through and is inert - Esc never quits).
/// `None` lets the key fall through to the global map (e.g. plain arrows -> move).
fn files_select_key(
    code: KeyCode,
    mods: KeyModifiers,
    view: &view_state::ViewState,
) -> Option<Msg> {
    if view.focus != view_state::Pane::Files {
        return None;
    }
    let shift = mods.contains(KeyModifiers::SHIFT);
    match code {
        // Space toggles the cursor file's mark (a dir marks its descendants).
        KeyCode::Char(' ') => Some(Msg::ToggleMark(view.files_sel)),
        // Shift+arrow extends the range from the anchor to the neighbour row.
        KeyCode::Down if shift => Some(Msg::SelectRange(view.files_sel.saturating_add(1))),
        KeyCode::Up if shift => Some(Msg::SelectRange(view.files_sel.saturating_sub(1))),
        // Esc clears an existing multi-selection; with none, falls through (inert).
        KeyCode::Esc if !view.files_marked.is_empty() => Some(Msg::ClearMarks),
        _ => None,
    }
}

/// Log-pane multi-COMMIT selection keys, active only while the Log pane is focused (so the
/// Files pane keeps its own). Mirrors `files_select_key`: Space toggles the cursor commit's
/// mark; Shift+Up/Down range-marks toward the neighbour; Esc clears. The keyboard path is the
/// drivable equivalent of Ctrl/Shift-click (which a terminal multiplexer may intercept).
/// `None` falls through to the global map (plain arrows -> move).
fn log_select_key(code: KeyCode, mods: KeyModifiers, view: &view_state::ViewState) -> Option<Msg> {
    if view.focus != view_state::Pane::Log {
        return None;
    }
    let shift = mods.contains(KeyModifiers::SHIFT);
    match code {
        KeyCode::Char(' ') => Some(Msg::ToggleCommitMark(view.log_sel)),
        KeyCode::Down if shift => Some(Msg::SelectCommitRange(view.log_sel.saturating_add(1))),
        KeyCode::Up if shift => Some(Msg::SelectCommitRange(view.log_sel.saturating_sub(1))),
        KeyCode::Esc if !view.commits_marked.is_empty() => Some(Msg::ClearMarks),
        _ => None,
    }
}

/// The Alt+letter button mnemonics: each fires the SAME `Msg` as clicking the
/// matching one-word toolbar button (the underlined letter in its title). The
/// accelerator letters are globally unique and mirror the locked label map in
/// `ui::layout` (`FILES_ACTIONS`, `PILLS`) and `ui::layout::filter_mnemonic`.
/// Returns `None` unless ALT is held, so a bare letter is never consumed here.
fn alt_accelerator(code: KeyCode, mods: KeyModifiers) -> Option<Msg> {
    if !mods.contains(KeyModifiers::ALT) {
        return None;
    }
    let KeyCode::Char(c) = code else {
        return None;
    };
    match c.to_ascii_lowercase() {
        // files toolbar
        'd' => Some(Msg::ToggleDiff),       // Diff
        'f' => Some(Msg::ToggleFlat),       // Flat
        'a' => Some(Msg::ToggleAllFiles),   // All
        'r' => Some(Msg::RequestRevert),    // Revert
        // toggles bar
        's' => Some(Msg::ToggleDiffMode),   // Side
        'w' => Some(Msg::ToggleWordWrap),   // Wrap
        'h' => Some(Msg::ToggleWhitespace), // Whitespace (underlined h)
        // filter dropdowns (same OpenDropdown a label click fires)
        'b' => Some(Msg::OpenDropdown(FilterKind::Branch)),
        'u' => Some(Msg::OpenDropdown(FilterKind::User)),
        't' => Some(Msg::OpenDropdown(FilterKind::Date)), // Date (underlined t)
        // repo-level git ops (these also live in the Git menu; kept as keyboard
        // accelerators now that the bottom action bar is gone)
        'c' => Some(Msg::OpenCommit),     // Commit
        'm' => Some(Msg::OpenAmend),      // aMend
        'g' => Some(Msg::OpenTag),        // taG
        'p' => Some(Msg::RequestPush),    // Push
        'l' => Some(Msg::RequestPull),    // puLl
        'e' => Some(Msg::RequestUpdate),  // updatE Project (the glyph-only toolbar refresh
        // button is a 3-cell target a human can miss; this is its drivable keyboard path)
        'y' => Some(Msg::OpenCopy), // copY
        'x' => Some(Msg::Quit),     // eXit
        _ => None,
    }
}

/// Editing keys (diff pane focused on an editable file). Ctrl combos do save +
/// clipboard (Ctrl+Q quits, Ctrl+S saves, Ctrl+C/X/V copy/cut/paste, Ctrl+A select
/// all). Otherwise text chars insert, the editing keys map to [`EditOp`]s, Esc leaves
/// editing, Tab indents. Shift + a movement key extends the selection. Every other
/// key is swallowed so nothing leaks to the pane shortcuts behind the buffer.
fn editor_key(code: KeyCode, mods: KeyModifiers) -> Option<Msg> {
    if mods.contains(KeyModifiers::CONTROL) {
        let shift = mods.contains(KeyModifiers::SHIFT);
        return match code {
            KeyCode::Char('q') => Some(Msg::Quit),
            KeyCode::Char('s') => Some(Msg::SaveEditor),
            KeyCode::Char('c') => Some(Msg::Edit(EditOp::Copy)),
            KeyCode::Char('x') => Some(Msg::Edit(EditOp::Cut)),
            KeyCode::Char('v') => Some(Msg::Edit(EditOp::Paste)),
            KeyCode::Char('a') => Some(Msg::Edit(EditOp::SelectAll)),
            // Undo / redo. Ctrl+Z undoes; Ctrl+Shift+Z and Ctrl+Y redo. A shifted
            // 'z' may arrive as 'z' + SHIFT or as a bare 'Z' (terminals that fold
            // Shift into the char and drop the modifier flag), so BOTH spellings map
            // to redo - the bare 'Z' arm covers the no-SHIFT-flag terminals.
            KeyCode::Char('z') if shift => Some(Msg::Edit(EditOp::Redo)),
            KeyCode::Char('Z') => Some(Msg::Edit(EditOp::Redo)),
            KeyCode::Char('z') => Some(Msg::Edit(EditOp::Undo)),
            KeyCode::Char('y') => Some(Msg::Edit(EditOp::Redo)),
            _ => None,
        };
    }
    if mods.contains(KeyModifiers::ALT) {
        return None;
    }
    // Shift + movement extends the selection from its anchor.
    let select = mods.contains(KeyModifiers::SHIFT);
    let mv = |dir| Some(Msg::Edit(EditOp::Move { dir, select }));
    match code {
        KeyCode::Esc => Some(Msg::DiffBlur),
        KeyCode::Enter => Some(Msg::Edit(EditOp::Newline)),
        KeyCode::Backspace => Some(Msg::Edit(EditOp::Backspace)),
        KeyCode::Delete => Some(Msg::Edit(EditOp::Delete)),
        KeyCode::Left => mv(Dir::Left),
        KeyCode::Right => mv(Dir::Right),
        KeyCode::Up => mv(Dir::Up),
        KeyCode::Down => mv(Dir::Down),
        KeyCode::Home => mv(Dir::Home),
        KeyCode::End => mv(Dir::End),
        KeyCode::PageUp => mv(Dir::PageUp),
        KeyCode::PageDown => mv(Dir::PageDown),
        KeyCode::Tab => Some(Msg::Edit(EditOp::Insert('\t'))),
        KeyCode::Char(c) => Some(Msg::Edit(EditOp::Insert(c))),
        _ => None,
    }
}

/// Revert-modal keys: `y`/Enter confirms, `n`/Esc cancels, EVERY other key is
/// swallowed (`None`) so the modal cannot leak input to the panes behind it.
fn modal_key(code: KeyCode) -> Option<Msg> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(Msg::ConfirmRevert),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Msg::CancelRevert),
        _ => None,
    }
}

/// Keys for the open action-bar dialog. An INPUT dialog types text (Backspace deletes,
/// Enter confirms, Esc cancels); a CONFIRM dialog takes y/Enter or n/Esc; the COPY
/// picker moves with Up/Down, Enter copies, Esc cancels. Unmapped keys are swallowed
/// (return `None`) so the dialog stays modal.
fn dialog_key(code: KeyCode, mods: KeyModifiers, view: &view_state::ViewState) -> Option<Msg> {
    use view_state::Dialog;
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let select = mods.contains(KeyModifiers::SHIFT);
    match view.dialog.as_ref()? {
        // A real editable line: caret movement, shift-select, clipboard (internal
        // register, like the editor), Tab toggles the optional checkbox.
        Dialog::Input { .. } => {
            if ctrl {
                return match code {
                    KeyCode::Char('c') => Some(Msg::DialogCopy),
                    KeyCode::Char('x') => Some(Msg::DialogCut),
                    KeyCode::Char('v') => Some(Msg::DialogPaste),
                    KeyCode::Char('a') => Some(Msg::DialogSelectAll),
                    _ => None,
                };
            }
            match code {
                KeyCode::Char(c) => Some(Msg::DialogInput(c)),
                KeyCode::Backspace => Some(Msg::DialogBackspace),
                KeyCode::Delete => Some(Msg::DialogDelete),
                KeyCode::Left => Some(Msg::DialogCaret { dir: Dir::Left, select }),
                KeyCode::Right => Some(Msg::DialogCaret { dir: Dir::Right, select }),
                KeyCode::Home => Some(Msg::DialogCaret { dir: Dir::Home, select }),
                KeyCode::End => Some(Msg::DialogCaret { dir: Dir::End, select }),
                // Tab cycles the archive format (rewriting the filename extension); on every
                // other input it toggles the optional checkbox.
                KeyCode::Tab => Some(match view.dialog.as_ref() {
                    Some(Dialog::Input { kind: view_state::InputKind::ArchiveProject, .. }) => {
                        Msg::DialogCycleArchiveFormat
                    }
                    _ => Msg::DialogToggleCheck,
                }),
                KeyCode::Enter => Some(Msg::DialogConfirm),
                KeyCode::Esc => Some(Msg::DialogCancel),
                _ => None,
            }
        }
        Dialog::Confirm { .. } => match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(Msg::DialogConfirm),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Msg::DialogCancel),
            _ => None,
        },
        Dialog::Copy { .. } | Dialog::Choice { .. } | Dialog::Picker { .. } | Dialog::RefPick { .. } => match code {
            KeyCode::Up | KeyCode::Char('k') => Some(Msg::DialogMove(-1)),
            KeyCode::Down | KeyCode::Char('j') => Some(Msg::DialogMove(1)),
            KeyCode::Enter => Some(Msg::DialogConfirm),
            KeyCode::Esc => Some(Msg::DialogCancel),
            _ => None,
        },
        // The rebase mark-items dialog: arrows (or k/j) move the focus, Space cycles the
        // focused row's verb, the p/s/f/d letters set it outright, Enter runs the rebase,
        // Esc cancels. (k/j are the nav keys; the verb letters never collide with them.)
        Dialog::Rebase { .. } => match code {
            KeyCode::Up | KeyCode::Char('k') => Some(Msg::DialogMove(-1)),
            KeyCode::Down | KeyCode::Char('j') => Some(Msg::DialogMove(1)),
            KeyCode::Char(' ') => Some(Msg::DialogCycleRow),
            KeyCode::Char('p') => Some(Msg::DialogSetRow(view_state::RebaseAction::Pick)),
            KeyCode::Char('s') => Some(Msg::DialogSetRow(view_state::RebaseAction::Squash)),
            KeyCode::Char('f') => Some(Msg::DialogSetRow(view_state::RebaseAction::Fixup)),
            KeyCode::Char('d') => Some(Msg::DialogSetRow(view_state::RebaseAction::Drop)),
            KeyCode::Enter => Some(Msg::DialogConfirm),
            KeyCode::Esc => Some(Msg::DialogCancel),
            _ => None,
        },
        // The read-only keybindings popup: any of the dismiss keys close it.
        Dialog::Help => match code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                Some(Msg::DialogCancel)
            }
            _ => None,
        },
        // Manage Remotes: arrows (or k/j) move the focus, `a` adds, `e`/Enter edits the URL,
        // `d` removes, Esc closes. (k/j navigate; a/e/d never collide with them.)
        Dialog::Remotes { .. } => match code {
            KeyCode::Up | KeyCode::Char('k') => Some(Msg::DialogMove(-1)),
            KeyCode::Down | KeyCode::Char('j') => Some(Msg::DialogMove(1)),
            KeyCode::Char('a') => Some(Msg::RemoteAddInput),
            KeyCode::Char('d') => Some(Msg::RemoteRemove),
            KeyCode::Char('e') | KeyCode::Enter => Some(Msg::DialogConfirm),
            KeyCode::Esc => Some(Msg::DialogCancel),
            _ => None,
        },
    }
}

/// Search-input keys: characters edit the query, Backspace deletes, Enter applies
/// and exits typing, Esc clears and exits. Other keys fall through (return `None`).
fn search_key(code: KeyCode) -> Option<Msg> {
    match code {
        KeyCode::Char(c) => Some(Msg::SearchPush(c)),
        KeyCode::Backspace => Some(Msg::SearchBackspace),
        KeyCode::Enter => Some(Msg::SearchBlur { clear: false }),
        KeyCode::Esc => Some(Msg::SearchBlur { clear: true }),
        _ => None,
    }
}

/// Files-pane search-input keys, mirroring [`search_key`] for the files filter field.
fn files_search_key(code: KeyCode) -> Option<Msg> {
    match code {
        KeyCode::Char(c) => Some(Msg::FilesSearchPush(c)),
        KeyCode::Backspace => Some(Msg::FilesSearchBackspace),
        KeyCode::Enter => Some(Msg::FilesSearchBlur { clear: false }),
        KeyCode::Esc => Some(Msg::FilesSearchBlur { clear: true }),
        _ => None,
    }
}

/// Open-dropdown keys: Up/Down move the highlight, Enter picks the highlighted
/// row, Esc closes. `sel` is the current highlight so Enter resolves to a row.
fn dropdown_key(code: KeyCode, sel: usize) -> Option<Msg> {
    match code {
        KeyCode::Down | KeyCode::Char('j') => Some(Msg::DropdownMove(1)),
        KeyCode::Up | KeyCode::Char('k') => Some(Msg::DropdownMove(-1)),
        KeyCode::Enter => Some(Msg::DropdownPick(sel)),
        KeyCode::Esc => Some(Msg::CloseDropdown),
        _ => None,
    }
}

/// Translate a physical key into a state intent. Keycodes stop here and never
/// reach [`AppState::apply`]; unmapped keys yield `None` (no redraw).
fn map_key(code: KeyCode, mods: KeyModifiers) -> Option<Msg> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    match code {
        // Exit is F10 (and the q / Ctrl+Q / Ctrl+C hatches). Esc is NOT a quit key: its
        // popup-close / leave-editing handlers run earlier in `route_key`, so a bare Esc
        // here (nothing to close) is inert rather than quitting the app.
        KeyCode::F(10) | KeyCode::Char('q') => Some(Msg::Quit),
        KeyCode::Char('c') if ctrl => Some(Msg::Quit),
        KeyCode::Tab | KeyCode::BackTab => Some(Msg::ToggleFocus),
        // -- pane resize (Ctrl+arrows nudge a split; the focused pane picks the
        // vertical-position divider). Guarded before the plain-arrow arms. --
        KeyCode::Down if ctrl => Some(Msg::NudgeFocusedVSplit(1)),
        KeyCode::Up if ctrl => Some(Msg::NudgeFocusedVSplit(-1)),
        KeyCode::Right if ctrl => Some(Msg::NudgeSplit(Divider::LogRight, 1)),
        KeyCode::Left if ctrl => Some(Msg::NudgeSplit(Divider::LogRight, -1)),
        KeyCode::Char('[') => Some(Msg::NudgeSplit(Divider::DiffOldNew, -1)),
        KeyCode::Char(']') => Some(Msg::NudgeSplit(Divider::DiffOldNew, 1)),
        KeyCode::Down | KeyCode::Char('j') => Some(Msg::Move(1)),
        KeyCode::Up | KeyCode::Char('k') => Some(Msg::Move(-1)),
        KeyCode::PageDown => Some(Msg::Move(10)),
        KeyCode::PageUp => Some(Msg::Move(-10)),
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right | KeyCode::Left => {
            Some(Msg::ToggleExpand)
        }
        // -- search + filters (bare-letter shortcuts; the Alt+letter button
        // mnemonics are resolved earlier in `route_key` via `alt_accelerator`). --
        KeyCode::Char('/') => Some(Msg::SearchFocus),
        KeyCode::Char('b') => Some(Msg::OpenDropdown(FilterKind::Branch)),
        KeyCode::Char('u') => Some(Msg::OpenDropdown(FilterKind::User)),
        KeyCode::Char('t') => Some(Msg::OpenDropdown(FilterKind::Date)),
        // -- view-option toggles (reflected in the toggles bar) --
        KeyCode::Char('d') => Some(Msg::ToggleDiff),
        KeyCode::Char('s') => Some(Msg::ToggleDiffMode),
        KeyCode::Char('w') => Some(Msg::ToggleWordWrap),
        KeyCode::Char('W') => Some(Msg::ToggleWhitespace),
        // -- repo verbs (lazygit-convention bare keys, mirrored on the bottom hint
        // bar; never reached while the editable diff owns typing). `S` for stash:
        // lazygit's lower `s` is taken by the side-by-side toggle above. --
        KeyCode::Char('c') => Some(Msg::OpenCommit),
        KeyCode::Char('p') => Some(Msg::RequestPull),
        KeyCode::Char('P') => Some(Msg::RequestPush),
        KeyCode::Char('S') => Some(Msg::MenuPick(view_state::MenuAction::GitStash)),
        KeyCode::Char('?') => Some(Msg::OpenHelp),
        // `Char('B')` (toggle the "In N branches" list) is handled in `route_key`,
        // where the layout map gates it on the link actually existing.
        _ => None,
    }
}

/// The `Msg` a hint-bar chip fires - the SAME intent its hotkey produces in
/// [`map_key`], so the bar and the keymap cannot drift.
fn hint_msg(key: view_state::HintKey) -> Msg {
    use view_state::HintKey;
    match key {
        HintKey::Commit => Msg::OpenCommit,
        HintKey::Pull => Msg::RequestPull,
        HintKey::Push => Msg::RequestPush,
        HintKey::Stash => Msg::MenuPick(view_state::MenuAction::GitStash),
        HintKey::Help => Msg::OpenHelp,
        HintKey::Quit => Msg::Quit,
    }
}

/// Translate a mouse event into a state intent against the current layout map.
/// Mirrors [`map_key`]: mouse events stop at this boundary and never reach
/// [`AppState::apply`]. Reads the layout (a pure `ui` type) but never `ui` state.
/// `drag` is transient runtime UI state: a left-press on a separator starts a
/// drag, the held `Drag` stream resizes, and `Up` ends it.
#[allow(clippy::too_many_arguments)]
fn map_mouse(
    ev: MouseEvent,
    lm: &LayoutMap,
    state: &AppState,
    drag: &mut Option<HandleKind>,
    clicks: &mut ClickTracker,
    edit_dragging: &mut bool,
    detail_dragging: &mut bool,
    diff_dragging: &mut bool,
    hbar_dragging: &mut bool,
    hbar_grab: &mut u16,
    edge_scroll: &mut Option<Dir>,
) -> Option<Msg> {
    let (col, row) = (ev.column, ev.row);
    // While the revert modal is open it is fully modal: a left click resolves only
    // its Yes/No buttons (anything else, including drags/wheel, is swallowed) so the
    // panes behind it cannot be interacted with.
    if let Some(ml) = &lm.revert_modal {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            if in_rect(col, row, ml.yes) {
                return Some(Msg::ConfirmRevert);
            }
            if in_rect(col, row, ml.no) {
                return Some(Msg::CancelRevert);
            }
        }
        return None;
    }
    // An action-bar dialog is fully modal: a left click resolves only its own controls
    // (the confirm / cancel buttons, the copy picker's option rows, an input checkbox);
    // every other click - including one outside the frame - is swallowed so the panes
    // behind it cannot be touched.
    if let Some(dl) = &lm.dialog {
        if let MouseEventKind::Down(MouseButton::Left) = ev.kind {
            if in_rect(col, row, dl.confirm) {
                return Some(Msg::DialogConfirm);
            }
            if in_rect(col, row, dl.cancel) {
                return Some(Msg::DialogCancel);
            }
            if dl.checkbox.is_some_and(|cb| in_rect(col, row, cb)) {
                return Some(Msg::DialogToggleCheck);
            }
            // `rows` are windowed at `dl.scroll` (the rebase list), so visible row j maps
            // to absolute row `scroll + j` - the same offset the renderer skipped.
            if let Some(j) = dl.rows.iter().position(|r| in_rect(col, row, *r)) {
                return Some(Msg::DialogPickRow(dl.scroll + j));
            }
        }
        return None;
    }
    match ev.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let n = clicks.register(col, row);
            *edit_dragging = false;
            *detail_dragging = false;
            *diff_dragging = false;
            *hbar_dragging = false;
            *edge_scroll = None;
            // An open commit context menu overlays the panes and owns the next
            // left-click: an item picks it, a click anywhere else dismisses it.
            // Resolve it before any pane gesture so the cells beneath are inert.
            if state.view.commit_menu.is_some() {
                return commit_menu_click(col, row, lm, state);
            }
            if state.view.files_menu.is_some() {
                return files_menu_click(col, row, lm, state);
            }
            // Likewise a menu-bar dropdown / filter dropdown / search-history popup
            // overlays the panes: route to `left_click` (which picks an item or
            // dismisses) BEFORE the diff/edit/detail gestures, else a dropdown item
            // drawn over the diff body is stolen by the text-selection gesture.
            if state.view.open_menu.is_some()
                || state.view.open_dropdown.is_some()
                || state.view.search_history_open
            {
                return left_click(col, row, ev.modifiers, lm, state);
            }
            if let Some(h) = hit_divider(col, row, lm) {
                *drag = Some(h);
                return drag_msg(h, col, row, lm);
            }
            // A press on the diff's horizontal scrollbar jumps to that offset and arms a
            // thumb drag (the Drag arm below tracks it). Checked first so the track row -
            // which sits just below the code rows - is never read as a text selection.
            if let Some((offset, grab)) = diff_hbar_pos(lm, state, col, row) {
                *drag = None;
                *hbar_dragging = true;
                *hbar_grab = grab;
                return Some(Msg::ScrollDiffH { offset });
            }
            // A press on the editable new side places the cursor there (and arms a
            // drag-select); a double-press selects the word, a triple the line.
            // `drag` stays None so the Drag arm below reads as a text selection.
            if let Some((r, c)) = edit_pos(lm, state, col, row) {
                *drag = None;
                *edit_dragging = true;
                return Some(match n {
                    2 => Msg::Edit(EditOp::SelectWord { row: r, col: c }),
                    k if k >= 3 => Msg::Edit(EditOp::SelectLine { row: r }),
                    _ => Msg::Edit(EditOp::Place { row: r, col: c, select: false }),
                });
            }
            // A press on a READ-ONLY diff body starts a character-level text selection (the
            // drag below extends it); `drag` stays None so the Drag arm reads it as one.
            if let Some((line, c)) = diff_pos(lm, state, col, row) {
                *drag = None;
                *diff_dragging = true;
                return Some(Msg::DiffSelectStart { line, col: c });
            }
            // A press in the commit-detail pane starts a read-only text selection (drag
            // extends it, a double-press selects the word, a triple the visual line). The
            // "Show all"/"Hide" branch link still wins as a click target (left_click).
            if in_rect(col, row, lm.detail)
                && !lm.branches_link.is_some_and(|r| in_rect(col, row, r))
            {
                if let Some(msg) = detail_down(lm, state, col, row, n, detail_dragging) {
                    return Some(msg);
                }
            }
            left_click(col, row, ev.modifiers, lm, state)
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            // A held scrollbar drag scrolls horizontally (the row is ignored, so a vertical
            // drift still tracks). A held separator drag resizes; a drag that STARTED on the
            // editable new side extends the editor selection; one started in the detail pane
            // extends the detail selection. A drag begun elsewhere is ignored so it cannot leak.
            if *hbar_dragging {
                return diff_hbar_drag(lm, state, col, *hbar_grab).map(|offset| Msg::ScrollDiffH { offset });
            }
            match (*drag, *edit_dragging, *detail_dragging, *diff_dragging) {
                (Some(h), _, _, _) => drag_msg(h, col, row, lm),
                (None, true, _, _) => {
                    let msg = edit_drag(lm, state, col, row);
                    // Remember an edge-hold: a terminal stops sending Drag events once the
                    // pointer is pinned at the pane edge, so the run loop auto-repeats this
                    // Move on each idle tick to keep scrolling + selecting (see run()).
                    *edge_scroll = edge_dir(&msg);
                    msg
                }
                (None, false, true, _) => detail_drag(lm, state, col, row),
                (None, false, false, true) => diff_drag(lm, state, col, row),
                _ => None,
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            *drag = None;
            *edit_dragging = false;
            *detail_dragging = false;
            *diff_dragging = false;
            *hbar_dragging = false;
            *edge_scroll = None;
            None
        }
        MouseEventKind::Down(MouseButton::Right) => right_click(col, row, lm, state),
        // Shift+vertical-wheel scrolls horizontally (the common terminal convention), but
        // FALLS BACK to a plain vertical scroll when there is no horizontal axis (not over
        // the diff, or word-wrap on) so the gesture is never swallowed over the log /
        // files / detail panes. A native horizontal wheel only ever scrolls the diff
        // sideways (no vertical fallback - a horizontal gesture over a list is a no-op).
        MouseEventKind::ScrollDown if ev.modifiers.contains(KeyModifiers::SHIFT) => {
            h_wheel(col, row, 1, lm, state).or_else(|| wheel(col, row, 1, lm, state))
        }
        MouseEventKind::ScrollUp if ev.modifiers.contains(KeyModifiers::SHIFT) => {
            h_wheel(col, row, -1, lm, state).or_else(|| wheel(col, row, -1, lm, state))
        }
        MouseEventKind::ScrollDown => wheel(col, row, 1, lm, state),
        MouseEventKind::ScrollUp => wheel(col, row, -1, lm, state),
        MouseEventKind::ScrollRight => h_wheel(col, row, 1, lm, state),
        MouseEventKind::ScrollLeft => h_wheel(col, row, -1, lm, state),
        _ => None,
    }
}

/// Right-click: over a commit row, open its context menu; over a files row, open the
/// files context menu; off any row, dismiss an open menu. `None` otherwise.
fn right_click(col: u16, row: u16, lm: &LayoutMap, state: &AppState) -> Option<Msg> {
    if in_x(col, lm.log_list) {
        // The log list includes the trailing "Load more history" footer row when more history
        // exists; it carries no context menu, so only a real commit row opens one.
        let count = state.visible_len() + state.repo.more_history as usize;
        if let Some(i) = row_to_index(lm.log_list, row, state.view.log_scroll, state.view.log_sel, count) {
            if i < state.visible_len() {
                return Some(Msg::OpenCommitMenu { index: i, col, row });
            }
        }
    }
    if in_x(col, lm.files_list) {
        if let Some(i) = row_to_index(
            lm.files_list,
            row,
            state.view.files_scroll,
            state.view.files_sel,
            state.files_rows_len(),
        ) {
            return Some(Msg::OpenFilesMenu { index: i, col, row });
        }
    }
    // Off any row: dismiss whichever context menu is open.
    if state.view.commit_menu.is_some() {
        return Some(Msg::CloseCommitMenu);
    }
    state.view.files_menu.as_ref().map(|_| Msg::CloseFilesMenu)
}

/// Resolve a left click while a commit context menu is open: an item row picks it, a
/// click anywhere else dismisses the menu. Every item is always available (reword on
/// an older commit just warns in its dialog).
fn commit_menu_click(col: u16, row: u16, lm: &LayoutMap, state: &AppState) -> Option<Msg> {
    let dl = lm.commit_menu.as_ref()?;
    let menu = state.view.commit_menu.as_ref()?;
    let leaves = menu.parent_rows();
    let n_leaves = leaves.len();
    // The open branch/tag fly-out overlays the parent, so test it FIRST. Its rows map to
    // `refs[ref_idx].actions[k]`; resolve the action here so the message carries it.
    if let Some(sub) = &dl.submenu {
        if let Some(k) = sub.items.iter().position(|r| in_rect(col, row, *r)) {
            if let Some(action) = menu.refs.get(sub.ref_idx).and_then(|rm| rm.actions.get(k)) {
                return Some(Msg::RefMenuPick { ref_idx: sub.ref_idx, action: *action });
            }
        }
    }
    if let Some(j) = dl.items.iter().position(|r| in_rect(col, row, *r)) {
        // The parent rows are windowed at `dl.scroll` (leaves then ref rows), so visible row
        // j maps to absolute parent index `scroll + j` - the shared contract with the layout.
        let abs = dl.scroll + j;
        if abs < n_leaves {
            return match leaves[abs] {
                view_state::CommitRow::Action(action, _) => Some(Msg::CommitMenuPick(action)),
                // A click on a group separator is inert - swallow it, keep the menu open.
                view_state::CommitRow::Sep => None,
            };
        }
        let ref_base = menu.ref_base();
        if abs < ref_base {
            return None; // the ref-section separator - inert, like the in-group rules.
        }
        return Some(Msg::OpenRefSubmenu { ref_idx: abs - ref_base });
    }
    Some(Msg::CloseCommitMenu)
}

/// Resolve a left click while the files context menu is open: an item row picks it, a
/// click anywhere else dismisses the menu. The item rects line up with `FilesMenu::items`.
fn files_menu_click(col: u16, row: u16, lm: &LayoutMap, state: &AppState) -> Option<Msg> {
    use gitgit::view_state::FilesRow;
    let dl = lm.files_menu.as_ref()?;
    let menu = state.view.files_menu.as_ref()?;
    if let Some(j) = dl.items.iter().position(|r| in_rect(col, row, *r)) {
        // The rows are windowed at `dl.scroll`, so visible row j maps to absolute row
        // `scroll + j` (the shared contract with the layout + render). A separator row is
        // inert (swallow the click); an action row dispatches.
        match menu.rows().get(dl.scroll + j) {
            Some(FilesRow::Action(action)) => return Some(Msg::FilesMenuPick(*action)),
            Some(FilesRow::Sep) => return None,
            None => {}
        }
    }
    Some(Msg::CloseFilesMenu)
}

/// A drag on the editable new side -> a `Place` edit op extending the selection from
/// the press point. `None` when there is no editable buffer or the point is outside
/// the editable text. Mirrors the diff render's geometry.
fn edit_click(lm: &LayoutMap, state: &AppState, col: u16, row: u16, select: bool) -> Option<Msg> {
    edit_pos(lm, state, col, row).map(|(r, c)| Msg::Edit(EditOp::Place { row: r, col: c, select }))
}

/// A held drag on the editable new side. When the pointer reaches the pane's RIGHT (or LEFT)
/// edge while a long line runs off-screen, extend the selection by one char in that direction
/// instead of placing at the clamped edge column: a `Move` clears the hscroll override so
/// cursor-follow reveals the hidden text, so dragging against the edge auto-scrolls the diff
/// and selects what scrolls into view. Away from the edges it is the normal place-extend.
/// The edge-scroll direction an `edit_drag` result encodes, or `None` for a plain in-pane
/// place. Lets the run loop auto-repeat the Move while the pointer is held at the edge.
fn edge_dir(msg: &Option<Msg>) -> Option<Dir> {
    match msg {
        Some(Msg::Edit(EditOp::Move { dir: d @ (Dir::Right | Dir::Left), select: true })) => Some(*d),
        _ => None,
    }
}

fn edit_drag(lm: &LayoutMap, state: &AppState, col: u16, row: u16) -> Option<Msg> {
    if let Some(dl) = lm.diff.as_ref() {
        if state.live_editor().is_some() {
            if let Some((left_x, right_x)) = ui::diff_view::editable_code_x_range(dl, &state.view) {
                if col >= right_x {
                    return Some(Msg::Edit(EditOp::Move { dir: Dir::Right, select: true }));
                }
                // Only scroll LEFT when code is actually hidden off the left (an active offset).
                if col <= left_x && state.view.diff_hscroll.unwrap_or(0) > 0 {
                    return Some(Msg::Edit(EditOp::Move { dir: Dir::Left, select: true }));
                }
            }
        }
    }
    edit_click(lm, state, col, row, true)
}

/// A press in the detail pane: map it to a wrapped `(row, col)` and emit the selection
/// intent (single -> start; double -> select word; triple -> select the visual line).
/// Arms `detail_dragging` so a following drag extends the selection. `None` when there
/// is no detail content.
fn detail_down(
    lm: &LayoutMap,
    state: &AppState,
    col: u16,
    row: u16,
    n: u32,
    detail_dragging: &mut bool,
) -> Option<Msg> {
    let d = state.repo.detail.as_ref()?;
    let (r, c) = ui::detail_panel::click_pos(d, &state.view, lm.detail, col, row)?;
    *detail_dragging = true;
    Some(match n {
        2 => {
            let (row, start, end) = ui::detail_panel::word_span(d, &state.view, lm.detail, r, c)?;
            Msg::DetailSelectWord { row, start, end }
        }
        k if k >= 3 => Msg::DetailSelectWord {
            row: r,
            start: 0,
            end: ui::detail_panel::line_len(d, &state.view, lm.detail, r),
        },
        _ => Msg::DetailSelectStart { row: r, col: c },
    })
}

/// A drag begun in the detail pane: extend the selection's cursor to the wrapped
/// `(row, col)` under the pointer (clamped to the content). `None` with no content.
fn detail_drag(lm: &LayoutMap, state: &AppState, col: u16, row: u16) -> Option<Msg> {
    let d = state.repo.detail.as_ref()?;
    let (r, c) = ui::detail_panel::click_pos(d, &state.view, lm.detail, col, row)?;
    Some(Msg::DetailSelectTo { row: r, col: c })
}

/// The `(logical diff-line index, character column)` under a click on a READ-ONLY diff
/// body, or `None` when the diff is the editable buffer (that uses [`edit_pos`]) or the
/// point lies outside it. The shared geometry for starting + extending the read-only
/// character selection.
fn diff_pos(lm: &LayoutMap, state: &AppState, col: u16, row: u16) -> Option<(usize, usize)> {
    // The editable <current> buffer owns its side (edit_pos / the editor selection); the
    // character selection is for read-only diffs only. A read-only inspect overlay masks the
    // editor (`live_editor` is None then), so a drag over the overlay selects characters.
    if state.live_editor().is_some() {
        return None;
    }
    let dl = lm.diff.as_ref()?;
    let d = match state.shown_view()? {
        gitgit::diff::FileView::Diff(d) => d,
        _ => return None,
    };
    ui::diff_view::locate_diff_click(dl, d, &state.view, col, row)
}

/// A drag over a read-only diff body extends the character selection to the cell under
/// the cursor. `None` when the drag did not begin on a read-only diff body.
fn diff_drag(lm: &LayoutMap, state: &AppState, col: u16, row: u16) -> Option<Msg> {
    let (line, c) = diff_pos(lm, state, col, row)?;
    Some(Msg::DiffSelectTo { line, col: c })
}

/// The diff body + preview for the scrollbar hit-test, or `None` when the diff is not a
/// `FileView::Diff` (a `Source`/binary/absent preview has no horizontal scrollbar).
fn diff_for_hbar<'a>(lm: &'a LayoutMap, state: &'a AppState) -> Option<(&'a ui::layout::DiffLayout, &'a gitgit::diff::FileDiff)> {
    let dl = lm.diff.as_ref()?;
    match state.shown_view()? {
        gitgit::diff::FileView::Diff(d) => Some((dl, d)),
        _ => None,
    }
}

/// The horizontal offset a press on the diff's scrollbar track maps to, or `None` when
/// the press is not on the track. Seeds a thumb drag.
fn diff_hbar_pos(lm: &LayoutMap, state: &AppState, col: u16, row: u16) -> Option<(usize, u16)> {
    let (dl, d) = diff_for_hbar(lm, state)?;
    let editor = state.live_editor();
    ui::diff_view::locate_diff_hbar(dl, d, &state.view, editor, col, row)
}

/// The horizontal offset for a continuing scrollbar drag at column `col` (row ignored, so
/// a vertical drift still scrolls), keeping the grab point `grab` under the pointer.
/// `None` when there is no scrollbar.
fn diff_hbar_drag(lm: &LayoutMap, state: &AppState, col: u16, grab: u16) -> Option<usize> {
    let (dl, d) = diff_for_hbar(lm, state)?;
    let editor = state.live_editor();
    ui::diff_view::diff_hbar_drag_offset(dl, d, &state.view, editor, col, grab)
}

/// Ctrl+C while a read-only diff line selection is live: compose the committed text and
/// copy it to the system clipboard. `None` lets Ctrl+C route normally (editor copy /
/// detail-pane copy / quit) when no diff selection is set.
fn diff_copy_key(code: KeyCode, mods: KeyModifiers, state: &AppState) -> Option<Msg> {
    if !mods.contains(KeyModifiers::CONTROL) || code != KeyCode::Char('c') {
        return None;
    }
    if state.view.dialog.is_some() || state.view.revert_confirm.is_some() {
        return None;
    }
    state.view.diff_sel?;
    let d = match state.shown_view()? {
        gitgit::diff::FileView::Diff(d) => d,
        _ => return None,
    };
    let text = ui::diff_view::selected_text(d, &state.view)?;
    Some(Msg::CopyText(text))
}

/// The 0-based buffer `(row, col)` under a click on the editable new side, or `None`
/// when there is no editable buffer / the point lies outside it. The shared geometry
/// for placing the cursor, drag-selecting, and word/line double/triple-click.
fn edit_pos(lm: &LayoutMap, state: &AppState, col: u16, row: u16) -> Option<(usize, usize)> {
    let dl = lm.diff.as_ref()?;
    // `live_editor` is None while a read-only inspect overlay is open, so a click on the
    // overlay never places a caret in the masked buffer.
    let editor = state.live_editor()?;
    let d = match state.shown_view()? {
        gitgit::diff::FileView::Diff(d) => d,
        _ => return None,
    };
    ui::diff_view::locate_edit_click(dl, d, editor, &state.view, col, row)
}

/// Tracks rapid same-cell left-presses so the editable diff can tell a single click
/// (place cursor) from a double (select word) or triple (select line). Resets when a
/// press lands on a different cell or after [`MULTI_CLICK`] elapses; a 4th press wraps
/// back to a single.
#[derive(Default)]
struct ClickTracker {
    last: Option<(u16, u16, Instant)>,
    count: u32,
}

/// Maximum gap between presses counted as part of the same multi-click.
const MULTI_CLICK: Duration = Duration::from_millis(400);

impl ClickTracker {
    /// Record a left-press at `(col, row)` and return the running click count
    /// (1 single, 2 double, 3 triple; wraps 3 -> 1).
    fn register(&mut self, col: u16, row: u16) -> u32 {
        let now = Instant::now();
        let same = self
            .last
            .is_some_and(|(c, r, t)| c == col && r == row && now.saturating_duration_since(t) <= MULTI_CLICK);
        self.count = if same { (self.count % 3) + 1 } else { 1 };
        self.last = Some((col, row, now));
        self.count
    }
}

/// The handle covering `(col, row)`, if any. Column handles are listed after the
/// pane separators, so a click on the column-vsep coincidence resolves the pane
/// split first - matching the visual stacking (the vsep sits at the list edge).
fn hit_divider(col: u16, row: u16, lm: &LayoutMap) -> Option<HandleKind> {
    lm.dividers
        .iter()
        .find(|d| in_rect(col, row, d.handle))
        .map(|d| d.id)
}

/// The drag message for handle `h` at the cursor: a pane split maps the coord to
/// a fraction of its parent (`SetSplit`). Out-of-range is fine; apply clamps.
fn drag_msg(h: HandleKind, col: u16, row: u16, lm: &LayoutMap) -> Option<Msg> {
    let dr = lm.dividers.iter().find(|x| x.id == h)?;
    match h {
        HandleKind::Pane(d) => {
            let frac = match dr.axis {
                SplitAxis::Vertical => {
                    col.saturating_sub(dr.parent.x) as f32 / dr.parent.width.max(1) as f32
                }
                SplitAxis::Horizontal => {
                    row.saturating_sub(dr.parent.y) as f32 / dr.parent.height.max(1) as f32
                }
            };
            Some(Msg::SetSplit(d, frac))
        }
    }
}

/// Resolve a left click in priority order: an open dropdown (option row or
/// dismiss), the left toolbar (search field / toggles / filter labels), the close
/// button, a toggle pill, a files-toolbar control, a log row, or a file row.
fn left_click(
    col: u16,
    row: u16,
    mods: KeyModifiers,
    lm: &LayoutMap,
    state: &AppState,
) -> Option<Msg> {
    if let Some(msg) = menu_dropdown_click(col, row, lm, &state.view) {
        return Some(msg);
    }
    if let Some(msg) = dropdown_click(col, row, lm) {
        return Some(msg);
    }
    if let Some(msg) = search_history_click(col, row, lm) {
        return Some(msg);
    }
    if let Some(msg) = toolbar_click(col, row, lm) {
        return Some(msg);
    }
    if in_rect(col, row, lm.close_btn) {
        return Some(Msg::Quit);
    }
    if let Some(menu) = lm.menus.iter().find(|m| in_rect(col, row, m.rect)) {
        // Click the open menu's own label again to close it (toggle); else open it.
        return Some(if state.view.open_menu == Some(menu.id) {
            Msg::CloseMenu
        } else {
            Msg::OpenMenu(menu.id)
        });
    }
    // A hint-bar chip fires the SAME Msg as its hotkey (the chips already exclude the
    // editing context, where the bar shows inert editor hints).
    if let Some((_, key)) = lm.hint_chips.iter().find(|(r, _)| in_rect(col, row, *r)) {
        return Some(hint_msg(*key));
    }
    // Files-pane search field: clear `x` and `.*` toggle first (they sit over the field
    // block), then the lens/text focus the field for typing.
    let fs = &lm.files_search;
    if fs.clear.is_some_and(|r| in_rect(col, row, r)) {
        return Some(Msg::FilesSearchClear);
    }
    if in_rect(col, row, fs.regex) {
        return Some(Msg::ToggleFilesRegex);
    }
    if in_rect(col, row, fs.lens) || in_rect(col, row, fs.field) {
        return Some(Msg::FilesSearchFocus);
    }
    if let Some(c) = lm.files_actions.iter().find(|c| in_rect(col, row, c.rect)) {
        return Some(files_action_msg(c.action));
    }
    if lm.branches_link.is_some_and(|r| in_rect(col, row, r)) {
        return Some(Msg::ToggleBranchList);
    }
    // The log selection indexes the FILTERED list, so the click bound is the visible count -
    // not the raw commit total - plus the trailing "Load more history" footer row when present.
    let more = state.repo.more_history;
    let log_count = state.visible_len() + more as usize;
    if let Some(i) = row_to_index(lm.log_list, row, state.view.log_scroll, state.view.log_sel, log_count) {
        if in_x(col, lm.log_list) {
            // A click on the footer row loads a deeper page; a real commit row selects it.
            if more && i == state.visible_len() {
                return Some(Msg::LoadMore);
            }
            // Ctrl-click toggles a commit into the multi-selection; Shift-click range-marks
            // from the cursor; a plain click selects the single commit (clearing the set).
            if mods.contains(KeyModifiers::CONTROL) {
                return Some(Msg::ToggleCommitMark(i));
            }
            if mods.contains(KeyModifiers::SHIFT) {
                return Some(Msg::SelectCommitRange(i));
            }
            return Some(Msg::SelectCommit(i));
        }
    }
    let files_len = state.files_rows_len();
    if let Some(i) = row_to_index(lm.files_list, row, state.view.files_scroll, state.view.files_sel, files_len) {
        if in_x(col, lm.files_list) {
            // A PLAIN left-click in the leading mark gutter toggles the row's mark -
            // no modifier needed, so multi-select works in every terminal (Shift+click
            // is eaten by most). Ctrl-click anywhere on the row also toggles (bonus
            // for terminals that forward it); Shift-click range-selects from the
            // anchor; a plain click on the body selects the row (folding a directory)
            // and clears any multi-selection.
            if in_x(col, lm.files_gutter) || mods.contains(KeyModifiers::CONTROL) {
                return Some(Msg::ToggleMark(i));
            }
            if mods.contains(KeyModifiers::SHIFT) {
                return Some(Msg::SelectRange(i));
            }
            return Some(Msg::ClickFile(i));
        }
    }
    None
}

/// Resolve a click while a top menu is open: an item row picks it; a click
/// anywhere else dismisses the popup. `None` when no menu is open.
fn menu_dropdown_click(col: u16, row: u16, lm: &LayoutMap, view: &view_state::ViewState) -> Option<Msg> {
    let dl = lm.menu_dropdown.as_ref()?;
    if let Some(i) = dl.items.iter().position(|r| in_rect(col, row, *r)) {
        // Row i maps 1:1 to `menu_rows[i]` (layout builds one rect per row); a Sep row is inert
        // (a click just closes the menu), an action fires unless disabled.
        let action = match view_state::menu_rows(dl.id).get(i) {
            Some(view_state::MenuRow::Action(a, _)) => *a,
            _ => return Some(Msg::CloseMenu),
        };
        // A disabled item (e.g. Undo with empty history) is inert: clicking it just
        // closes the menu rather than firing a silent no-op MenuPick.
        if !view.menu_action_enabled(action) {
            return Some(Msg::CloseMenu);
        }
        return Some(Msg::MenuPick(action));
    }
    Some(Msg::CloseMenu)
}

/// Resolve a click while a filter dropdown is open: an option row picks it; a
/// click anywhere else dismisses the popup. `None` when no dropdown is open.
fn dropdown_click(col: u16, row: u16, lm: &LayoutMap) -> Option<Msg> {
    let dl = lm.dropdown.as_ref()?;
    if let Some(j) = dl.options.iter().position(|r| in_rect(col, row, *r)) {
        // `options[j]` is the j-th VISIBLE row; add the scroll offset for the absolute
        // option index (the list may be scrolled when capped).
        return Some(Msg::DropdownPick(dl.scroll + j));
    }
    Some(Msg::CloseDropdown)
}

/// Resolve a click while the lens's recent-search history popup is open: a row runs
/// that query; a click anywhere else closes the popup. `None` when it is not open.
fn search_history_click(col: u16, row: u16, lm: &LayoutMap) -> Option<Msg> {
    let layout = lm.search_history.as_ref()?;
    if let Some(i) = layout.options.iter().position(|r| in_rect(col, row, *r)) {
        return Some(Msg::PickSearchHistory(i));
    }
    Some(Msg::ToggleSearchHistory) // a click outside the popup closes it
}

/// Resolve a click on the toolbar: the search field focuses it, the `.*`/`Cc`
/// cells toggle match modes, and a filter label opens its dropdown.
fn toolbar_click(col: u16, row: u16, lm: &LayoutMap) -> Option<Msg> {
    let tb = &lm.toolbar_ui;
    // Lens opens the recent-search history; the `x` (shown only with a query) clears it.
    if in_rect(col, row, tb.search_lens) {
        return Some(Msg::ToggleSearchHistory);
    }
    if tb.search_clear.is_some_and(|r| in_rect(col, row, r)) {
        return Some(Msg::SearchClear);
    }
    if in_rect(col, row, tb.regex_toggle) {
        return Some(Msg::ToggleRegex);
    }
    if in_rect(col, row, tb.search_field) {
        return Some(Msg::SearchFocus);
    }
    if in_rect(col, row, tb.refresh_btn) {
        return Some(Msg::RequestUpdate);
    }
    tb.filter_labels
        .iter()
        .find(|(_, r)| in_rect(col, row, *r))
        .map(|(kind, _)| Msg::OpenDropdown(*kind))
}

/// Resolve a wheel scroll: over the diff body it scrolls the preview (or the editable
/// buffer's viewport WITHOUT moving the caret); over the detail pane it scrolls the
/// detail content; over the log or files list it scrolls the viewport WITHOUT moving the
/// selection. Every pane scrolls independently of its cursor/selection now.
fn wheel(col: u16, row: u16, delta: isize, lm: &LayoutMap, state: &AppState) -> Option<Msg> {
    // An open filter dropdown captures the wheel while the pointer is over it: scroll its
    // (capped) option list by moving the highlight one option per notch.
    if let Some(dd) = &lm.dropdown {
        if in_rect(col, row, dd.frame) {
            return Some(Msg::DropdownMove(delta.signum()));
        }
    }
    // An open commit context menu captures the wheel over its frame: window its items so
    // a menu taller than the terminal reveals its clipped (destructive) bottom actions.
    if let Some(cm) = &lm.commit_menu {
        if in_rect(col, row, cm.frame) {
            let max_scroll = ui::layout::menu_max_scroll(cm.rows, cm.frame.height);
            if max_scroll == 0 {
                return None; // everything fits; nothing to scroll
            }
            let next = (cm.scroll as isize + delta.signum()).clamp(0, max_scroll as isize) as usize;
            return Some(Msg::ScrollCommitMenu { offset: next });
        }
    }
    // The files context menu captures the wheel the same way: a tall menu (the full 15-row
    // `<current>` file menu on a short terminal) windows its rows so Rollback/Delete stay
    // reachable.
    if let Some(fm) = &lm.files_menu {
        if in_rect(col, row, fm.frame) {
            let max_scroll = ui::layout::menu_max_scroll(fm.rows, fm.frame.height);
            if max_scroll == 0 {
                return None; // everything fits; nothing to scroll
            }
            let next = (fm.scroll as isize + delta.signum()).clamp(0, max_scroll as isize) as usize;
            return Some(Msg::ScrollFilesMenu { offset: next });
        }
    }
    // A modal dialog captures the wheel: over a scrollable list dialog (the rebase steps, the
    // compare picker) it moves the selection one row per notch (windowing the list via the
    // layout's scroll, like the filter dropdown); anywhere else it is SWALLOWED so the wheel
    // never scrolls the panes behind the modal.
    if let Some(dd) = &lm.dialog {
        if !dd.rows.is_empty() && in_rect(col, row, dd.frame) {
            return Some(Msg::DialogMove(delta.signum()));
        }
        return None;
    }
    if let Some(dl) = &lm.diff {
        if in_rect(col, row, dl.body_old) || dl.body_new.is_some_and(|b| in_rect(col, row, b)) {
            // Editing: scroll the editable diff's VIEWPORT by whole rows, leaving the
            // caret where it is (it may scroll out of view; any edit refollows it). A
            // read-only inspect overlay masks the editor (`live_editor` None) -> the
            // ScrollDiff path scrolls the overlay's own lines.
            if state.live_editor().is_some() {
                return edit_scroll_msg(lm, state, delta);
            }
            return Some(Msg::ScrollDiff {
                delta,
                pane_height: dl.body_old.height as usize,
            });
        }
    }
    if in_rect(col, row, lm.detail) {
        // Source the WRAPPED line count (not the logical one) so a folded subject /
        // long signature scrolls all the way to its last visual row.
        let content_height = state
            .repo
            .detail
            .as_ref()
            .map_or(0, |d| ui::detail_panel::wrapped_lines(d, &state.view, lm.detail).len());
        return Some(Msg::ScrollDetail {
            delta,
            pane_height: lm.detail.height as usize,
            content_height,
        });
    }
    if in_rect(col, row, lm.log_list) {
        // Count the trailing "Load more history" footer row so the wheel can scroll it into view.
        let offset = list_scroll_offset(
            state.view.log_scroll,
            state.view.log_sel,
            state.visible_len() + state.repo.more_history as usize,
            lm.log_list.height as usize,
            delta,
        );
        return Some(Msg::ScrollLog { offset });
    }
    if in_rect(col, row, lm.files_list) {
        let offset = list_scroll_offset(
            state.view.files_scroll,
            state.view.files_sel,
            state.files_rows_len(),
            lm.files_list.height as usize,
            delta,
        );
        return Some(Msg::ScrollFiles { offset });
    }
    None
}

/// A horizontal-wheel tick over the diff body (word-wrap off): scroll the code sideways
/// WITHOUT moving the caret. Works for both the committed (browse) and editable diff;
/// seeds from the current offset (the wheel override when set, else the editing
/// cursor-follow / browse column 0), steps by `dir * HSCROLL_COLS`, clamps to the
/// longest line. `None` over no diff body, when wrapping, or when nothing overflows.
fn h_wheel(col: u16, row: u16, dir: isize, lm: &LayoutMap, state: &AppState) -> Option<Msg> {
    let dl = lm.diff.as_ref()?;
    let over_body = in_rect(col, row, dl.body_old) || dl.body_new.is_some_and(|b| in_rect(col, row, b));
    if !over_body {
        return None;
    }
    let d = match state.shown_view()? {
        gitgit::diff::FileView::Diff(d) => d,
        _ => return None,
    };
    let (cur, max) = ui::diff_view::hscroll_bounds(dl, d, &state.view, state.live_editor())?;
    let next = (cur as isize + dir * HSCROLL_COLS).clamp(0, max as isize) as usize;
    Some(Msg::ScrollDiffH { offset: next })
}

/// Keyboard horizontal scroll of the diff body (word-wrap off): scroll the code sideways
/// WITHOUT moving the caret. The fix for a READ-ONLY (browsed) diff, where there is no
/// caret to drive cursor-follow and most terminals eat shift/native horizontal wheel - so
/// a long line was otherwise unreachable. Also an Alt+arrow JUMP on the editable diff, so a
/// long line is reachable without scrubbing the caret one column at a time. On the editable
/// diff only Alt+arrow scrolls (bare arrows move the caret); on a read-only diff the bare
/// arrows + Home/End scroll. Resolved at the runtime boundary (it needs the layout + preview
/// geometry `apply` must not see). `None` lets the key route normally.
fn diff_hscroll_key(code: KeyCode, mods: KeyModifiers, state: &AppState, lm: &LayoutMap) -> Option<Msg> {
    use view_state::Pane;
    let v = &state.view;
    // Never shadow an overlay's own keys (a dialog/modal/menu/dropdown/search/history).
    if v.focus != Pane::Diff
        || v.word_wrap
        || mods.contains(KeyModifiers::CONTROL)
        || v.dialog.is_some()
        || v.revert_confirm.is_some()
        || v.search_active
        || v.open_menu.is_some()
        || v.open_dropdown.is_some()
        || v.commit_menu.is_some()
        || v.files_menu.is_some()
        || v.files_search_active
        || v.search_history_open
    {
        return None;
    }
    let alt = mods.contains(KeyModifiers::ALT);
    // A read-only inspect overlay browses like a committed diff (no editable caret), so the
    // bare arrows scroll it; `live_editor` is None then.
    let editing = state.live_editor().is_some();
    let dl = lm.diff.as_ref()?;
    let d = match state.shown_view()? {
        gitgit::diff::FileView::Diff(d) => d,
        _ => return None,
    };
    let (cur, max) = ui::diff_view::hscroll_bounds(dl, d, v, state.live_editor())?;
    if max == 0 {
        return None; // nothing overflows; let the key stay inert / route normally.
    }
    let step = |dir: isize| (cur as isize + dir * HSCROLL_COLS).clamp(0, max as isize) as usize;
    let offset = match code {
        KeyCode::Left if alt => step(-1),
        KeyCode::Right if alt => step(1),
        KeyCode::Left if !editing => step(-1),
        KeyCode::Right if !editing => step(1),
        KeyCode::Home if !editing => 0,
        KeyCode::End if !editing => max,
        _ => return None,
    };
    Some(Msg::ScrollDiffH { offset })
}

/// A wheel tick over the editable diff: scroll the viewport by whole rows WITHOUT moving
/// the caret. Seeds from the current offset (the free-scroll override when set, else the
/// cursor-follow offset), steps by the wheel amount, and clamps so the last row pins to
/// the bottom. `None` when there is no measurable editable buffer (nothing to scroll).
fn edit_scroll_msg(lm: &LayoutMap, state: &AppState, delta: isize) -> Option<Msg> {
    let dl = lm.diff.as_ref()?;
    let editor = state.live_editor()?;
    let d = match state.shown_view()? {
        gitgit::diff::FileView::Diff(d) => d,
        _ => return None,
    };
    let (cur, max) = ui::diff_view::edit_scroll_bounds(dl, d, editor, &state.view)?;
    let top = (cur as isize + delta * WHEEL_LINES).clamp(0, max as isize) as usize;
    Some(Msg::ScrollEdit { top })
}

/// The next first-visible row for a wheel tick over a selection list (log / files): step
/// the current offset (the free-scroll override when set, else the selection-follow
/// offset) by the wheel amount, clamped so the last row pins to the bottom. The selection
/// is left untouched, so it may scroll out of view (a navigation key refollows it).
fn list_scroll_offset(scroll: Option<usize>, sel: usize, len: usize, window: usize, delta: isize) -> usize {
    let cur = scroll.unwrap_or_else(|| ui::layout::list_offset(sel, len, window));
    let max = len.saturating_sub(window);
    (cur as isize + delta * WHEEL_LINES).clamp(0, max as isize) as usize
}

/// Map a files-toolbar action to its message.
fn files_action_msg(action: ui::layout::FilesAction) -> Msg {
    use ui::layout::FilesAction;
    match action {
        FilesAction::Flat => Msg::ToggleFlat,
        FilesAction::AllFiles => Msg::ToggleAllFiles,
        FilesAction::Focus => Msg::FocusOpenFile,
    }
}

/// Whether `(col, row)` lies within `r`.
fn in_rect(col: u16, row: u16, r: Rect) -> bool {
    col >= r.x && col < r.right() && row >= r.y && row < r.bottom()
}

/// Whether `col` lies within `r`'s horizontal span (row already matched).
fn in_x(col: u16, r: Rect) -> bool {
    col >= r.x && col < r.right()
}

/// Whether an event must force a repaint without producing a `Msg`. The
/// fraction-based layout already adapts to `frame.area()`, but the loop redraws
/// only when `dirty`; a terminal resize would otherwise leave the splits/columns
/// stale, so it forces exactly one redraw at the new size. Pure classifier, so the
/// rule is unit-testable away from the live loop.
fn forces_redraw(ev: &Event) -> bool {
    matches!(ev, Event::Resize(..))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lazygit-convention repo verbs: bare keys that fire the SAME intents as the
    /// Git menu, plus `?` for the keybindings popup. (`S` because lower `s` is the
    /// side-by-side toggle.) These only resolve via `map_key`, which the editing
    /// branch of `route_key` never reaches - the editor keeps typing letters.
    #[test]
    fn lazygit_verb_keys_map_to_their_git_intents() {
        let none = KeyModifiers::NONE;
        assert!(matches!(map_key(KeyCode::Char('c'), none), Some(Msg::OpenCommit)));
        assert!(matches!(map_key(KeyCode::Char('p'), none), Some(Msg::RequestPull)));
        assert!(matches!(map_key(KeyCode::Char('P'), none), Some(Msg::RequestPush)));
        assert!(matches!(
            map_key(KeyCode::Char('S'), none),
            Some(Msg::MenuPick(view_state::MenuAction::GitStash))
        ));
        assert!(matches!(map_key(KeyCode::Char('?'), none), Some(Msg::OpenHelp)));
        // The chip mapping mirrors the keymap one-for-one.
        assert!(matches!(hint_msg(view_state::HintKey::Commit), Msg::OpenCommit));
        assert!(matches!(hint_msg(view_state::HintKey::Quit), Msg::Quit));
    }

    #[test]
    fn help_dialog_dismisses_on_every_close_key() {
        let mut v = view_state::ViewState::new(0);
        v.dialog = Some(view_state::Dialog::Help);
        let none = KeyModifiers::NONE;
        for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Char('?'), KeyCode::Char('q')] {
            assert!(
                matches!(dialog_key(code, none, &v), Some(Msg::DialogCancel)),
                "{code:?} closes the help popup"
            );
        }
        assert!(dialog_key(KeyCode::Char('x'), none, &v).is_none(), "other keys swallowed");
    }

    #[test]
    fn resize_forces_redraw_other_events_do_not() {
        // A resize must flip `dirty` so the layout reflows at the new size; a focus
        // change (an event the loop ignores) must NOT trigger a redraw on its own.
        assert!(forces_redraw(&Event::Resize(120, 40)), "a resize forces a redraw");
        assert!(
            !forces_redraw(&Event::FocusGained),
            "a non-resize passive event does not force a redraw"
        );
    }

    /// A view with an input dialog open (with the new-branch checkbox), for routing tests.
    fn view_with_input_dialog() -> view_state::ViewState {
        use view_state::{Dialog, InputKind, TextField};
        let mut v = view_state::ViewState::new(0);
        v.dialog = Some(Dialog::Input {
            kind: InputKind::NewBranch,
            field: TextField::new("name".to_string()),
            commit: Some("abc123".to_string()),
            note: None,
            checkbox: Some(("Checkout new branch".to_string(), true)),
        });
        v
    }

    fn ctrl(c: char) -> (KeyCode, KeyModifiers) {
        (KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn dialog_key_input_clipboard_and_edit_routing() {
        let v = view_with_input_dialog();
        let none = KeyModifiers::NONE;
        // Clipboard (Ctrl+C/X/V/A) routes to the dialog's edit ops, NOT a quit.
        assert!(matches!(dialog_key(ctrl('c').0, ctrl('c').1, &v), Some(Msg::DialogCopy)));
        assert!(matches!(dialog_key(ctrl('x').0, ctrl('x').1, &v), Some(Msg::DialogCut)));
        assert!(matches!(dialog_key(ctrl('v').0, ctrl('v').1, &v), Some(Msg::DialogPaste)));
        assert!(matches!(dialog_key(ctrl('a').0, ctrl('a').1, &v), Some(Msg::DialogSelectAll)));
        // Char inserts; Backspace / Delete edit; Tab toggles the checkbox.
        assert!(matches!(dialog_key(KeyCode::Char('z'), none, &v), Some(Msg::DialogInput('z'))));
        assert!(matches!(dialog_key(KeyCode::Delete, none, &v), Some(Msg::DialogDelete)));
        assert!(matches!(dialog_key(KeyCode::Tab, none, &v), Some(Msg::DialogToggleCheck)));
    }

    #[test]
    fn dialog_key_caret_and_shift_select() {
        let v = view_with_input_dialog();
        let none = KeyModifiers::NONE;
        let shift = KeyModifiers::SHIFT;
        assert!(matches!(
            dialog_key(KeyCode::Left, none, &v),
            Some(Msg::DialogCaret { dir: Dir::Left, select: false })
        ));
        assert!(matches!(
            dialog_key(KeyCode::Right, shift, &v),
            Some(Msg::DialogCaret { dir: Dir::Right, select: true })
        ));
        assert!(matches!(
            dialog_key(KeyCode::Home, none, &v),
            Some(Msg::DialogCaret { dir: Dir::Home, select: false })
        ));
    }

    #[test]
    fn route_key_modal_quit_is_ctrl_q_not_ctrl_c() {
        let v = view_with_input_dialog();
        // The dialog branch of route_key returns before reading the layout, but the
        // signature needs one - build a real map over an empty repo.
        let lm = compute_layout(Rect::new(0, 0, 80, 24), &gitgit::model::RepoModel::empty(), &v);
        // Ctrl+Q is the modal escape hatch; Ctrl+C must NOT quit (it copies in-dialog).
        assert!(matches!(route_key(ctrl('q').0, ctrl('q').1, &v, &lm), Some(Msg::Quit)));
        assert!(matches!(route_key(ctrl('c').0, ctrl('c').1, &v, &lm), Some(Msg::DialogCopy)));
    }

    #[test]
    fn exit_is_f10_not_esc() {
        let none = KeyModifiers::NONE;
        // F10 quits; the q / Ctrl+Q / Ctrl+C hatches still quit.
        assert!(matches!(map_key(KeyCode::F(10), none), Some(Msg::Quit)));
        assert!(matches!(map_key(KeyCode::Char('q'), none), Some(Msg::Quit)));
        assert!(matches!(map_key(ctrl('q').0, ctrl('q').1), Some(Msg::Quit)));
        // Esc is NOT a quit key (its popup-close / leave-editing handlers run in route_key;
        // a bare Esc reaching map_key is inert).
        assert!(!matches!(map_key(KeyCode::Esc, none), Some(Msg::Quit)));
    }
}
