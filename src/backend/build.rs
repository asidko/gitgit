//! Pure backend builders: the git2-free helpers that shape raw repository facts
//! into owned `model` types. The real backend (`git.rs`) extracts plain strings
//! from libgit2 and hands them here; nothing in this module touches git2, so the
//! shaping logic is unit-testable without a repository.
//!
//! Three pure responsibilities, one function each:
//!   * `subject_spans` - split a commit summary into link/text spans (the log's
//!     blue-URL rendering),
//!   * `tree_from_paths` - fold a flat changed-file list into the collapsed,
//!     directory-grouped [`TreeNode`] tree the files pane renders,
//!   * `format_when` - the single epoch+offset -> "DD.MM.YYYY, HH:MM" formatter.

use crate::config::DateFormat;
use crate::model::{FileStatus, SubjectSpan, SubjectTone, TreeNode};

/// Split a commit subject into spans, marking embedded `http(s)://` URLs as links
/// so the log renders them blue. A bare subject yields one non-link span; a
/// subject with a URL yields up to three (text, link, text) per URL. PURE.
pub fn subject_spans(subject: &str) -> Vec<SubjectSpan> {
    let mut spans = Vec::new();
    let mut rest = subject;
    while let Some(start) = find_url(rest) {
        if start > 0 {
            spans.push(span(&rest[..start], false));
        }
        let url = &rest[start..];
        let end = url_end(url);
        spans.push(span(&url[..end], true));
        rest = &url[end..];
    }
    if !rest.is_empty() || spans.is_empty() {
        spans.push(span(rest, false));
    }
    spans
}

/// Byte offset of the next `http://`/`https://` in `s`, or `None`.
fn find_url(s: &str) -> Option<usize> {
    let http = s.find("http://");
    let https = s.find("https://");
    match (http, https) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

/// Length of the URL run at the start of `s`: everything up to the first
/// whitespace (URLs in commit subjects are space-delimited).
fn url_end(s: &str) -> usize {
    s.find(char::is_whitespace).unwrap_or(s.len())
}

fn span(text: &str, link: bool) -> SubjectSpan {
    SubjectSpan {
        text: text.to_string(),
        tone: if link { SubjectTone::Link } else { SubjectTone::Plain },
    }
}

/// Fold a flat list of (path, status) changed files into a collapsed,
/// directory-grouped tree, matching the fixture layout: directories carry their
/// recursive file count, single-child directory chains collapse into one
/// `"a/b/c"` node, and every directory starts expanded. PURE; deterministic order
/// (input order preserved, directories before files within a level is NOT imposed
/// - the caller passes git's already-sorted delta order).
pub fn tree_from_paths(paths: &[(String, FileStatus)]) -> Vec<TreeNode> {
    let mut root = DirBuilder::default();
    for (path, status) in paths {
        root.insert(&split_path(path), *status);
    }
    collapse(root.into_nodes())
}

/// Split a `/`-separated path into its non-empty components.
fn split_path(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// A mutable directory under construction: ordered children so the output tree
/// preserves git's delta order rather than a hashmap's arbitrary one.
#[derive(Default)]
struct DirBuilder {
    /// (name, subtree) for child directories, in first-seen order.
    dirs: Vec<(String, DirBuilder)>,
    /// (name, status) for leaf files, in first-seen order.
    files: Vec<(String, FileStatus)>,
}

impl DirBuilder {
    /// Insert one file (its path components) into this directory subtree.
    fn insert(&mut self, components: &[&str], status: FileStatus) {
        match components {
            [] => {}
            [name] => self.files.push((name.to_string(), status)),
            [head, tail @ ..] => self.child_dir(head).insert(tail, status),
        }
    }

    /// Borrow (creating if absent) the child directory named `name`.
    fn child_dir(&mut self, name: &str) -> &mut DirBuilder {
        if let Some(idx) = self.dirs.iter().position(|(n, _)| n == name) {
            return &mut self.dirs[idx].1;
        }
        self.dirs.push((name.to_string(), DirBuilder::default()));
        &mut self.dirs.last_mut().unwrap().1
    }

    /// Recursive count of leaf files under this directory.
    fn file_count(&self) -> usize {
        self.files.len() + self.dirs.iter().map(|(_, d)| d.file_count()).sum::<usize>()
    }

    /// Convert this directory's children into `TreeNode`s (dirs first as built,
    /// then files), without collapsing single-child chains - that is `collapse`'s
    /// job so it can fold names across the dir/node boundary.
    fn into_nodes(self) -> Vec<TreeNode> {
        let mut out = Vec::with_capacity(self.dirs.len() + self.files.len());
        for (name, dir) in self.dirs {
            let file_count = dir.file_count();
            out.push(TreeNode::Dir {
                name,
                file_count,
                expanded: true,
                children: dir.into_nodes(),
            });
        }
        for (name, status) in self.files {
            out.push(TreeNode::File { name, status });
        }
        out
    }
}

/// Collapse single-child directory chains into one `"a/b/c"` node (GoLand-style
/// path grouping): a directory whose only child is another directory merges names
/// with it. Files and multi-child directories are left as-is. Applied recursively.
fn collapse(nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    nodes.into_iter().map(collapse_node).collect()
}

/// Collapse one node: a directory with a lone directory child folds its name into
/// that child and recurses; everything else recurses into its children unchanged.
fn collapse_node(node: TreeNode) -> TreeNode {
    match node {
        TreeNode::File { .. } => node,
        TreeNode::Dir {
            name,
            file_count,
            expanded,
            children,
        } => {
            // Lone directory child -> merge "name/child" and re-collapse.
            if children.len() == 1 {
                if let Some(TreeNode::Dir {
                    name: child_name, ..
                }) = children.first()
                {
                    let merged = format!("{name}/{child_name}");
                    let TreeNode::Dir {
                        children: grandchildren,
                        ..
                    } = children.into_iter().next().unwrap()
                    else {
                        unreachable!("matched a Dir above");
                    };
                    return collapse_node(TreeNode::Dir {
                        name: merged,
                        file_count,
                        expanded,
                        children: grandchildren,
                    });
                }
            }
            TreeNode::Dir {
                name,
                file_count,
                expanded,
                children: collapse(children),
            }
        }
    }
}

/// Days in each month of `year` (Gregorian), index 0 = January.
fn days_in_months(year: i64) -> [i64; 12] {
    let feb = if is_leap(year) { 29 } else { 28 };
    [31, feb, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Format a git timestamp (`epoch` seconds UTC + `offset_minutes` from UTC) for the
/// log/detail in the commit's own local zone, per `fmt`. The default
/// [`DateFormat::Dmy`] reproduces the prior `"DD.MM.YYYY, HH:MM"` string exactly.
/// Hand-rolled civil-from-days so the crate stays dependency-light. PURE.
pub fn format_when(epoch: i64, offset_minutes: i32, fmt: DateFormat) -> String {
    let (year, month, day) = civil_from_days(local_days(epoch, offset_minutes));
    let (hour, minute) = local_hm(epoch, offset_minutes);
    match fmt {
        DateFormat::Dmy => format!("{day:02}.{month:02}.{year:04}, {hour:02}:{minute:02}"),
        DateFormat::Iso => format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}"),
    }
}

/// Today's date as `YYYY-MM-DD` from the system clock (UTC). The runtime seeds
/// `view.today` with this at boot so the zero-IO `apply` (and the golden snapshot) stay
/// clock-free; it is the `<current>` zip-archive prefill's date suffix.
pub fn today_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_date(now, 0, crate::config::DateFormat::Iso)
}

/// The DATE-only portion of [`format_when`] (no time) in the commit's own local zone,
/// per `fmt` - `"DD.MM.YYYY"` (Dmy) or `"YYYY-MM-DD"` (Iso). Used by the revision picker
/// label, where a compact date disambiguates same-subject commits without the time noise.
/// PURE.
pub fn format_date(epoch: i64, offset_minutes: i32, fmt: DateFormat) -> String {
    let (year, month, day) = civil_from_days(local_days(epoch, offset_minutes));
    match fmt {
        DateFormat::Dmy => format!("{day:02}.{month:02}.{year:04}"),
        DateFormat::Iso => format!("{year:04}-{month:02}-{day:02}"),
    }
}

/// The log-column label: "Today, HH:MM" / "Yesterday, HH:MM" when the commit's local
/// calendar day is now's / the previous day, else the absolute [`format_when`] string.
/// The day comparison uses the COMMIT's own UTC offset for both sides, so it needs no
/// viewer-timezone data and stays a pure function of its inputs. `now_epoch` is the
/// load-time wall clock (UTC seconds); 0 (clock unavailable) just yields absolute dates.
pub fn format_when_relative(epoch: i64, offset_minutes: i32, now_epoch: i64, fmt: DateFormat) -> String {
    let (hour, minute) = local_hm(epoch, offset_minutes);
    match local_days(now_epoch, offset_minutes) - local_days(epoch, offset_minutes) {
        0 => format!("Today, {hour:02}:{minute:02}"),
        1 => format!("Yesterday, {hour:02}:{minute:02}"),
        _ => format_when(epoch, offset_minutes, fmt),
    }
}

/// Days since the Unix epoch for a timestamp in its own local zone.
fn local_days(epoch: i64, offset_minutes: i32) -> i64 {
    (epoch + offset_minutes as i64 * 60).div_euclid(86_400)
}

/// `(hour, minute)` of a timestamp in its own local zone.
fn local_hm(epoch: i64, offset_minutes: i32) -> (i64, i64) {
    let secs = (epoch + offset_minutes as i64 * 60).rem_euclid(86_400);
    (secs / 3600, (secs % 3600) / 60)
}

/// Convert a day count since the Unix epoch (1970-01-01) into a `(year, month,
/// day)` Gregorian civil date. Walks years then months; the range we format
/// (commit dates) keeps the loop trivially bounded.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let mut year = 1970;
    let mut remaining = days;
    loop {
        let year_len = if is_leap(year) { 366 } else { 365 };
        if remaining >= 0 && remaining < year_len {
            break;
        }
        if remaining < 0 {
            year -= 1;
            remaining += if is_leap(year) { 366 } else { 365 };
        } else {
            remaining -= year_len;
            year += 1;
        }
    }
    let months = days_in_months(year);
    let mut month = 0;
    while remaining >= months[month] {
        remaining -= months[month];
        month += 1;
    }
    (year, month as i64 + 1, remaining + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_spans_splits_url() {
        let spans = subject_spans("Merge branch of https://x.test/repo int");
        assert_eq!(spans.len(), 3);
        assert!(spans[0].tone == SubjectTone::Plain && spans[0].text == "Merge branch of ");
        assert!(spans[1].tone == SubjectTone::Link && spans[1].text == "https://x.test/repo");
        assert!(spans[2].tone == SubjectTone::Plain && spans[2].text == " int");
    }

    #[test]
    fn subject_spans_plain_subject_is_one_span() {
        let spans = subject_spans("just a normal subject");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].tone, SubjectTone::Plain);
    }

    #[test]
    fn tree_collapses_single_child_chains() {
        let paths = vec![
            ("a/b/c/file.go".to_string(), FileStatus::Modified),
            ("a/b/c/other.go".to_string(), FileStatus::Added),
        ];
        let tree = tree_from_paths(&paths);
        assert_eq!(tree.len(), 1);
        match &tree[0] {
            TreeNode::Dir { name, file_count, children, .. } => {
                assert_eq!(name, "a/b/c", "single-child chain collapsed");
                assert_eq!(*file_count, 2);
                assert_eq!(children.len(), 2, "two leaf files under the collapsed dir");
            }
            _ => panic!("expected a directory root"),
        }
    }

    #[test]
    fn format_when_epoch_and_offset() {
        // 2026-05-22 12:08 in +0000 (the fixture style), then a +120-minute zone.
        // 2026-05-22T12:08:00Z = 1779451680.
        assert_eq!(format_when(1_779_451_680, 0, DateFormat::Dmy), "22.05.2026, 12:08");
        assert_eq!(format_when(1_779_451_680, 120, DateFormat::Dmy), "22.05.2026, 14:08");
    }

    #[test]
    fn format_when_unix_epoch() {
        assert_eq!(format_when(0, 0, DateFormat::Dmy), "01.01.1970, 00:00");
    }

    #[test]
    fn format_when_iso_variant() {
        // The ISO alternative renders the same instant as "YYYY-MM-DD HH:MM".
        assert_eq!(format_when(1_779_451_680, 0, DateFormat::Iso), "2026-05-22 12:08");
    }
}
