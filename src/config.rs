//! Boot-time TOML configuration: the behavioral + layout params, read ONCE at
//! startup and threaded into the composition root. This module owns parsing
//! ([`Config::from_toml_str`], PURE) and the boot filesystem read ([`Config::load`],
//! the ONLY IO here). It never mutates after boot: `apply` stays ZERO-IO and `ui`
//! never sees a `Config`.
//!
//! Single source of the defaults: every field's `Default` reproduces the prior
//! hardcoded value (`ViewState::new` splits/columns/view + `git.rs` caps + the
//! `format_when` date string), so a MISSING or PARTIAL config file renders
//! byte-for-byte like today. `#[serde(default)]` on every field fills any gap.
//!
//! Theme/color palette is deliberately OUT OF SCOPE: a later milestone threads a
//! `&Theme` through `ui`; no color/theme global lives here.

use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;

use crate::view_state::{DiffMode, ViewState, SEARCH_HISTORY_MAX, SPLIT_MAX, SPLIT_MIN};

// -- defaults: the SINGLE source mirrors the prior consts ---------------------
//
// One named const per default value, the one home of each. `#[serde(default)]` on
// every section makes serde fill missing fields from that section's `Default`
// impl (built from these consts), so the "default == prior const" invariant has
// exactly one definition to guard, and a missing/partial file renders like today.

const DEFAULT_SPLIT_DIFF_V: f32 = 0.50;
const DEFAULT_SPLIT_LOG_H: f32 = 0.62;
const DEFAULT_SPLIT_RIGHT_V: f32 = 0.58;
const DEFAULT_SPLIT_DIFF_H: f32 = 0.50;
const DEFAULT_MOUSE: bool = true;
const DEFAULT_COMMIT_CAP: usize = 300;
const DEFAULT_PREVIEW_LINE_CAP: usize = 5000;

/// A config parse failure carrying the human-readable TOML error. Surfaced to the
/// user as a non-fatal `Status::Error` warning; never panics, never aborts boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// The diff body layout, mirroring [`view_state::DiffMode`] with serde-friendly
/// `side`/`unified` spellings. A config-local enum so `view_state` carries no
/// serde derive; [`DiffModeCfg::into_view`] is the single conversion home.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffModeCfg {
    #[default]
    Side,
    Unified,
}

impl DiffModeCfg {
    /// Map to the view-state mode the UI renders.
    fn into_view(self) -> DiffMode {
        match self {
            DiffModeCfg::Side => DiffMode::SideBySide,
            DiffModeCfg::Unified => DiffMode::Unified,
        }
    }
}

/// The commit-date format. A small closed set (not a full strftime) keeps the
/// crate dependency-light; the default reproduces the prior `"DD.MM.YYYY, HH:MM"`
/// string exactly. [`DateFormat::format`] is the single formatting home the real
/// backend calls in place of the bare `format_when`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DateFormat {
    /// `"DD.MM.YYYY, HH:MM"` - the prior hardcoded default.
    #[default]
    Dmy,
    /// `"YYYY-MM-DD HH:MM"` - ISO-style alternative.
    Iso,
}

/// `[repo]` section: which repository to open.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct RepoConfig {
    /// Repository path; `None` (default) -> the current working directory.
    pub path: Option<String>,
}

/// `[layout]` section: the four pane-separator fractions. Defaults equal
/// `ViewState::new`; clamped to `[SPLIT_MIN, SPLIT_MAX]` when applied.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    pub split_diff_v: f32,
    pub split_log_h: f32,
    pub split_right_v: f32,
    pub split_diff_h: f32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            split_diff_v: DEFAULT_SPLIT_DIFF_V,
            split_log_h: DEFAULT_SPLIT_LOG_H,
            split_right_v: DEFAULT_SPLIT_RIGHT_V,
            split_diff_h: DEFAULT_SPLIT_DIFF_H,
        }
    }
}

/// `[view]` section: the initial view-option toggles. Defaults equal
/// `ViewState::new` (side-by-side diff shown, no wrap, no whitespace markers).
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ViewConfig {
    pub diff_mode: DiffModeCfg,
    pub word_wrap: bool,
    pub show_whitespace: bool,
    pub show_diff: bool,
    /// Autosave the editable buffer when navigating away / on Esc. Default true.
    pub autosave: bool,
    /// Files pane FLAT (no-folder) list vs the nested tree. Default false (tree).
    pub flat: bool,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            diff_mode: DiffModeCfg::default(),
            word_wrap: false,
            show_whitespace: false,
            show_diff: true,
            autosave: true,
            flat: false,
        }
    }
}

/// `[behavior]` section: runtime/backend params not tied to layout. Defaults equal
/// the prior `main.rs` mouse-capture choice + the `git.rs` caps + date format.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    pub mouse: bool,
    pub commit_cap: usize,
    pub preview_line_cap: usize,
    pub date_format: DateFormat,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            mouse: DEFAULT_MOUSE,
            commit_cap: DEFAULT_COMMIT_CAP,
            preview_line_cap: DEFAULT_PREVIEW_LINE_CAP,
            date_format: DateFormat::default(),
        }
    }
}

/// The whole boot-time configuration. `Default::default()` reproduces today's
/// hardcoded behavior byte-for-byte; `#[serde(default)]` on every section means a
/// missing or partial file fills the rest from `Default`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub repo: RepoConfig,
    pub layout: LayoutConfig,
    pub view: ViewConfig,
    pub behavior: BehaviorConfig,
}

impl Config {
    /// Parse a TOML document into a `Config`. PURE (no IO): the testable seam.
    /// Unknown keys are tolerated by serde's defaults; a malformed document is an
    /// `Err` the caller surfaces as a warning.
    pub fn from_toml_str(s: &str) -> Result<Config, ConfigError> {
        toml::from_str(s).map_err(|e| ConfigError(e.to_string()))
    }

    /// Read the boot config from disk, in precedence order, returning the resolved
    /// `Config` plus an OPTIONAL warning string. The ONLY filesystem touch in this
    /// module, run ONCE at boot (sub-ms): it does NOT block the first frame.
    ///
    /// Order: `./gitgit.toml` (project-local) wins over the user config at
    /// `$XDG_CONFIG_HOME/gitgit/config.toml` (falling back to
    /// `~/.config/gitgit/config.toml`). A MISSING file is silent -> `Default` with
    /// no warning. A PRESENT but MALFORMED file is NON-FATAL: it yields `Default`
    /// plus a warning the caller renders as `Status::Error`, NEVER a panic.
    pub fn load() -> (Config, Option<String>) {
        for path in config_paths() {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    return match Config::from_toml_str(&text) {
                        Ok(cfg) => (cfg, None),
                        Err(e) => (
                            Config::default(),
                            Some(format!("config {}: {e}", path.display())),
                        ),
                    };
                }
                // Not present at this location -> try the next candidate.
                Err(_) => continue,
            }
        }
        (Config::default(), None)
    }

    /// The repository path to open: the `[repo].path` override, or `None` -> the
    /// caller's cwd. Keeps the cwd default decision out of the boot wiring.
    pub fn repo_path(&self) -> Option<PathBuf> {
        self.repo.path.as_ref().map(PathBuf::from)
    }

    /// The configured date format the real backend uses to render commit dates.
    pub fn date_format(&self) -> DateFormat {
        self.behavior.date_format
    }
}

/// Candidate config paths in precedence order: the project-local file first, then
/// the user config under `$XDG_CONFIG_HOME` (or `~/.config`). Pure path assembly;
/// the existence check happens in [`Config::load`].
fn config_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("gitgit.toml")];
    if let Some(dir) = user_config_dir() {
        paths.push(dir.join("gitgit").join("config.toml"));
    }
    paths
}

/// The XDG user-config directory: `$XDG_CONFIG_HOME`, else `$HOME/.config`, else
/// `None` (no user config location resolvable).
fn user_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
}

/// Path to the remembered-layout file (`<config dir>/gitgit/state.toml`): beside the
/// user config but SEPARATE from it, so the user's hand-authored config is never
/// rewritten. `None` when no config home resolves.
pub fn state_path() -> Option<PathBuf> {
    user_config_dir().map(|d| d.join("gitgit").join("state.toml"))
}

/// Runtime-adjusted geometry remembered across runs: the pane split fractions a user
/// drags (the log right-hand columns are auto-fit, so they are no longer persisted).
/// Every field optional so a partial/older file still loads (a stale `author`/`hash`/
/// `date` key from an older build is simply ignored by serde). Overlaid onto the view
/// at boot (winning over config defaults) and rewritten on quit, so a resize sticks.
#[derive(Debug, Default, Deserialize)]
pub struct LayoutState {
    pub split_diff_v: Option<f32>,
    pub split_log_h: Option<f32>,
    pub split_right_v: Option<f32>,
    pub split_diff_h: Option<f32>,
    /// Recent search queries (most-recent first). Persisted so the lens history popup
    /// survives restarts.
    pub search_history: Option<Vec<String>>,
    /// The View/Editor/files TOGGLES, remembered across runs (every field optional so an
    /// older state file still loads). A runtime toggle is written here on quit and overlaid
    /// at boot, WINNING over the config defaults - so flipping word-wrap (etc.) sticks without
    /// rewriting the user-authored config.toml. `side_by_side` encodes `DiffMode`.
    pub side_by_side: Option<bool>,
    pub word_wrap: Option<bool>,
    pub show_whitespace: Option<bool>,
    pub show_diff: Option<bool>,
    pub hide_unchanged: Option<bool>,
    pub autosave: Option<bool>,
    pub files_flat: Option<bool>,
    pub show_all_files: Option<bool>,
    pub show_blame: Option<bool>,
}

/// Read the remembered layout, or `None` when absent / unreadable / malformed
/// (best-effort: a bad state file is ignored, never fatal). Parse-only, like
/// [`Config::load`].
pub fn load_layout_state() -> Option<LayoutState> {
    let text = std::fs::read_to_string(state_path()?).ok()?;
    toml::from_str(&text).ok()
}

/// Persist the view's current drag-adjustable geometry to [`state_path`] (best-effort:
/// creates the dir, ignores IO errors). Hand-written TOML because our `toml` build is
/// parse-only. Called on quit so the next run reopens at the same proportions.
pub fn save_layout_state(view: &ViewState) {
    let Some(path) = state_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let history = view
        .search_history
        .iter()
        .map(|q| format!("\"{}\"", toml_escape(q)))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        "# gitgit remembered layout - auto-written on exit. Delete to reset to config defaults.\n\
         split_diff_v = {}\nsplit_log_h = {}\nsplit_right_v = {}\nsplit_diff_h = {}\n\
         search_history = [{}]\n\
         side_by_side = {}\nword_wrap = {}\nshow_whitespace = {}\nshow_diff = {}\n\
         hide_unchanged = {}\nautosave = {}\nfiles_flat = {}\nshow_all_files = {}\nshow_blame = {}\n",
        view.split_diff_v,
        view.split_log_h,
        view.split_right_v,
        view.split_diff_h,
        history,
        view.diff_mode == crate::view_state::DiffMode::SideBySide,
        view.word_wrap,
        view.show_whitespace,
        view.show_diff,
        view.hide_unchanged,
        view.autosave,
        view.files_flat,
        view.show_all_files,
        view.show_blame,
    );
    let _ = std::fs::write(path, body);
}

/// Escape a string into a TOML basic (double-quoted) string body. Our `toml` build is
/// parse-only, so the writer hand-rolls this; the reader is `toml::from_str`. Backslash
/// and quote are escaped; every control char (TOML forbids them raw in a basic string)
/// becomes `\uXXXX` - so even an unexpected byte can NEVER produce an invalid document
/// that makes `load_layout_state` drop ALL remembered geometry, not just the history.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            _ => out.push(c),
        }
    }
    out
}

impl ViewState {
    /// Build the initial view state from `config`, reusing the EXISTING clamp so an
    /// out-of-range TOML split is clamped (not panicked) into bounds. The one home
    /// that maps the layout/view sections onto a fresh `ViewState`; `files_sel` is
    /// still the backend's default-selection seed. (The log right-hand columns are
    /// auto-fit at render time, so they take no config.)
    pub fn from_config(config: &Config, files_sel: usize) -> Self {
        let mut view = ViewState::new(files_sel);
        view.split_diff_v = config.layout.split_diff_v.clamp(SPLIT_MIN, SPLIT_MAX);
        view.split_log_h = config.layout.split_log_h.clamp(SPLIT_MIN, SPLIT_MAX);
        view.split_right_v = config.layout.split_right_v.clamp(SPLIT_MIN, SPLIT_MAX);
        view.split_diff_h = config.layout.split_diff_h.clamp(SPLIT_MIN, SPLIT_MAX);
        view.diff_mode = config.view.diff_mode.into_view();
        view.word_wrap = config.view.word_wrap;
        view.show_whitespace = config.view.show_whitespace;
        view.show_diff = config.view.show_diff;
        view.autosave = config.view.autosave;
        view.files_flat = config.view.flat;
        // The opened repository's directory basename - the zip-archive default filename's
        // `<repo>` part - derived once here from the configured (or current-dir) repo path.
        let root = config
            .repo_path()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        view.repo_root_name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".to_string());
        view
    }

    /// Overlay a remembered [`LayoutState`] onto the view, clamping each present value
    /// with the SAME bounds as [`Self::from_config`] so a hand-edited state file cannot
    /// push a split out of range. Absent fields keep the config-derived value.
    pub fn overlay_layout_state(&mut self, st: &LayoutState) {
        if let Some(v) = st.split_diff_v {
            self.split_diff_v = v.clamp(SPLIT_MIN, SPLIT_MAX);
        }
        if let Some(v) = st.split_log_h {
            self.split_log_h = v.clamp(SPLIT_MIN, SPLIT_MAX);
        }
        if let Some(v) = st.split_right_v {
            self.split_right_v = v.clamp(SPLIT_MIN, SPLIT_MAX);
        }
        if let Some(v) = st.split_diff_h {
            self.split_diff_h = v.clamp(SPLIT_MIN, SPLIT_MAX);
        }
        if let Some(history) = &st.search_history {
            // Trust but bound: drop blanks and cap at the same limit the runtime keeps.
            self.search_history = history
                .iter()
                .filter(|q| !q.trim().is_empty())
                .take(SEARCH_HISTORY_MAX)
                .cloned()
                .collect();
        }
        // Remembered toggles WIN over the config defaults (a runtime flip persisted here).
        if let Some(v) = st.side_by_side {
            self.diff_mode = if v { DiffMode::SideBySide } else { DiffMode::Unified };
        }
        if let Some(v) = st.word_wrap {
            self.word_wrap = v;
        }
        if let Some(v) = st.show_whitespace {
            self.show_whitespace = v;
        }
        if let Some(v) = st.show_diff {
            self.show_diff = v;
        }
        if let Some(v) = st.hide_unchanged {
            self.hide_unchanged = v;
        }
        if let Some(v) = st.autosave {
            self.autosave = v;
        }
        if let Some(v) = st.files_flat {
            self.files_flat = v;
        }
        if let Some(v) = st.show_all_files {
            self.show_all_files = v;
        }
        if let Some(v) = st.show_blame {
            self.show_blame = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view_state::ViewState;

    #[test]
    fn toml_escape_handles_quotes_and_backslashes() {
        assert_eq!(toml_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(toml_escape("plain"), "plain");
    }

    #[test]
    fn search_history_survives_a_quote_round_trip() {
        // The hand-written writer + the toml reader must agree on escaping.
        let q = "a\"b\\c regex.*";
        let doc = format!("search_history = [\"{}\"]", toml_escape(q));
        let st: LayoutState = toml::from_str(&doc).unwrap();
        assert_eq!(st.search_history, Some(vec![q.to_string()]));
    }

    #[test]
    fn toml_escape_neutralizes_control_chars_no_whole_file_corruption() {
        // A raw control char must NOT produce an invalid document (which would drop ALL
        // persisted geometry on load). It escapes to \uXXXX and round-trips intact.
        let q = "a\u{1}b\u{7f}c";
        let doc = format!(
            "split_log_h = 0.5\nsearch_history = [\"{}\"]",
            toml_escape(q)
        );
        let st: LayoutState = toml::from_str(&doc).expect("control char must stay valid TOML");
        assert_eq!(st.split_log_h, Some(0.5), "the rest of the document still parses");
        assert_eq!(st.search_history, Some(vec![q.to_string()]));
    }

    #[test]
    fn stale_column_keys_from_an_older_state_file_are_ignored() {
        // An older build wrote author/hash/date keys; the column widths are now auto-fit,
        // so those keys must parse harmlessly (serde ignores unknown fields) and the live
        // split + history keys still load.
        let doc = "author = 12\nhash = 9\ndate = 18\nsplit_log_h = 0.4\n";
        let st: LayoutState = toml::from_str(doc).expect("stale keys stay valid TOML");
        assert_eq!(st.split_log_h, Some(0.4), "the live keys still load");
    }

    #[test]
    fn persisted_view_toggles_round_trip_and_win_over_config() {
        use crate::view_state::DiffMode;
        // A view with NON-default toggles (the opposite of config defaults) writes a state body
        // that, parsed + overlaid onto a fresh config-default view, restores every toggle.
        let mut saved = ViewState::new(0);
        saved.diff_mode = DiffMode::Unified;
        saved.word_wrap = true;
        saved.show_whitespace = true;
        saved.show_diff = false;
        saved.hide_unchanged = true;
        saved.autosave = false;
        saved.files_flat = true;
        saved.show_all_files = true;
        // Emulate save_layout_state's body (the fields under test) and parse it back.
        let doc = format!(
            "side_by_side = {}\nword_wrap = {}\nshow_whitespace = {}\nshow_diff = {}\n\
             hide_unchanged = {}\nautosave = {}\nfiles_flat = {}\nshow_all_files = {}\n",
            saved.diff_mode == DiffMode::SideBySide,
            saved.word_wrap,
            saved.show_whitespace,
            saved.show_diff,
            saved.hide_unchanged,
            saved.autosave,
            saved.files_flat,
            saved.show_all_files,
        );
        let st: LayoutState = toml::from_str(&doc).expect("toggle state is valid TOML");
        let mut restored = ViewState::new(0); // config defaults (side-by-side, no wrap, show_diff, autosave)
        restored.overlay_layout_state(&st);
        assert_eq!(restored.diff_mode, DiffMode::Unified, "diff mode persisted");
        assert!(restored.word_wrap, "word wrap persisted");
        assert!(restored.show_whitespace, "whitespace persisted");
        assert!(!restored.show_diff, "show_diff persisted");
        assert!(restored.hide_unchanged, "hide_unchanged persisted");
        assert!(!restored.autosave, "autosave persisted");
        assert!(restored.files_flat, "flat persisted");
        assert!(restored.show_all_files, "all-files persisted");
    }

    #[test]
    fn overlay_applies_persisted_search_history_bounded() {
        let doc = r#"search_history = ["beta", "alpha", "", "  "]"#;
        let st: LayoutState = toml::from_str(doc).unwrap();
        let mut view = ViewState::new(0);
        view.overlay_layout_state(&st);
        assert_eq!(
            view.search_history,
            vec!["beta".to_string(), "alpha".to_string()],
            "blank entries dropped, most-recent order preserved"
        );
    }

    /// Config::default() MUST equal the prior hardcoded consts (ViewState::new +
    /// git.rs caps + the format_when default), guarding byte-identity: no config
    /// file reproduces today's behavior exactly.
    #[test]
    fn config_default_matches_current_consts() {
        let cfg = Config::default();
        let view = ViewState::new(0);
        // Layout splits == ViewState::new.
        assert_eq!(cfg.layout.split_diff_v, view.split_diff_v);
        assert_eq!(cfg.layout.split_log_h, view.split_log_h);
        assert_eq!(cfg.layout.split_right_v, view.split_right_v);
        assert_eq!(cfg.layout.split_diff_h, view.split_diff_h);
        // View toggles == ViewState::new.
        assert_eq!(cfg.view.diff_mode.into_view(), view.diff_mode);
        assert_eq!(cfg.view.word_wrap, view.word_wrap);
        assert_eq!(cfg.view.show_whitespace, view.show_whitespace);
        assert_eq!(cfg.view.show_diff, view.show_diff);
        assert_eq!(cfg.view.autosave, view.autosave, "autosave on by default");
        assert_eq!(cfg.view.flat, view.files_flat, "files tree (not flat) by default");
        // Behavior == prior main.rs/git.rs values.
        assert!(cfg.behavior.mouse, "mouse capture on by default");
        assert_eq!(cfg.behavior.commit_cap, 300, "COMMIT_CAP default");
        assert_eq!(cfg.behavior.preview_line_cap, 5000, "PREVIEW_LINE_CAP default");
        assert_eq!(cfg.behavior.date_format, DateFormat::Dmy, "DD.MM.YYYY default");
        // Repo path defaults to None -> cwd.
        assert!(cfg.repo.path.is_none(), "no repo override by default");
    }

    /// A TOML with only `[layout].split_diff_v` still yields default layout/view/
    /// behavior: every other field falls back to Default via `#[serde(default)]`.
    #[test]
    fn from_toml_str_partial_fills_defaults() {
        let cfg = Config::from_toml_str("[layout]\nsplit_diff_v = 0.42\n").unwrap();
        let d = Config::default();
        assert_eq!(cfg.layout.split_diff_v, 0.42, "the set field takes");
        // Everything else is Default.
        assert_eq!(cfg.layout.split_log_h, d.layout.split_log_h);
        assert_eq!(cfg.view.diff_mode, d.view.diff_mode);
        assert_eq!(cfg.view.show_diff, d.view.show_diff);
        assert_eq!(cfg.behavior.commit_cap, d.behavior.commit_cap);
        assert_eq!(cfg.behavior.date_format, d.behavior.date_format);
        assert!(cfg.repo.path.is_none());
    }

    /// Every section parses into the right field with the right type.
    #[test]
    fn from_toml_str_full_roundtrip() {
        let toml = r#"
            [repo]
            path = "/tmp/some/repo"

            [layout]
            split_diff_v = 0.40
            split_log_h = 0.55
            split_right_v = 0.60
            split_diff_h = 0.45

            [view]
            diff_mode = "unified"
            word_wrap = true
            show_whitespace = true
            show_diff = false
            autosave = false
            flat = true

            [behavior]
            mouse = false
            commit_cap = 1000
            preview_line_cap = 250
            date_format = "iso"
        "#;
        let cfg = Config::from_toml_str(toml).unwrap();
        assert_eq!(cfg.repo.path.as_deref(), Some("/tmp/some/repo"));
        assert_eq!(cfg.layout.split_diff_v, 0.40);
        assert_eq!(cfg.layout.split_log_h, 0.55);
        assert_eq!(cfg.layout.split_right_v, 0.60);
        assert_eq!(cfg.layout.split_diff_h, 0.45);
        assert_eq!(cfg.view.diff_mode, DiffModeCfg::Unified);
        assert!(cfg.view.word_wrap);
        assert!(cfg.view.show_whitespace);
        assert!(!cfg.view.show_diff);
        assert!(!cfg.view.autosave, "autosave parsed off");
        assert!(cfg.view.flat, "flat parsed on");
        assert!(!cfg.behavior.mouse);
        assert_eq!(cfg.behavior.commit_cap, 1000);
        assert_eq!(cfg.behavior.preview_line_cap, 250);
        assert_eq!(cfg.behavior.date_format, DateFormat::Iso);
    }

    /// Malformed TOML is an `Err` (never a panic). `Config::load` mirrors this:
    /// a present-but-bad file returns `Default` + a warning - asserted here via the
    /// pure `from_toml_str` since `load()` touches the filesystem.
    #[test]
    fn from_toml_str_malformed_is_err() {
        // Unterminated string / not valid TOML.
        let err = Config::from_toml_str("this is = = not toml [[[");
        assert!(err.is_err(), "malformed TOML yields Err, not a panic");
        // The load() contract: a parse Err -> Default + a warning. Emulate the
        // exact branch load() runs on a malformed file without touching the fs.
        let (cfg, warning) = match Config::from_toml_str("nonsense ===") {
            Ok(cfg) => (cfg, None),
            Err(e) => (Config::default(), Some(e.to_string())),
        };
        assert!(warning.is_some(), "a malformed file surfaces a warning");
        // Falls back to the byte-identical defaults despite the bad file.
        assert_eq!(cfg.layout.split_log_h, Config::default().layout.split_log_h);
    }

    /// An out-of-range split in the config is CLAMPED by `from_config` (reusing the
    /// existing bounds), never panicked or left out of range.
    #[test]
    fn view_state_apply_config_clamps() {
        let toml = r#"
            [layout]
            split_log_h = 5.0
            split_diff_v = -1.0
        "#;
        let cfg = Config::from_toml_str(toml).unwrap();
        let view = ViewState::from_config(&cfg, 0);
        assert_eq!(view.split_log_h, SPLIT_MAX, "over-range split clamps to SPLIT_MAX");
        assert_eq!(view.split_diff_v, SPLIT_MIN, "under-range split clamps to SPLIT_MIN");
    }
}
