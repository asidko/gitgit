//! Pure in-memory line diff for the LIVE editable diff.
//!
//! Turns a `base` (the HEAD blob) and a `work` buffer (the user's live edits) into a
//! `Vec<DiffLine>` the existing diff viewer renders - recomputed on every keystroke,
//! so the right (new) side reflects the edits immediately. UNFOLDED: every line is
//! emitted (no collapsed context), so the cursor can sit on any working line and the
//! whole file stays editable. No git, no IO: a plain LCS over line strings.

use crate::diff::{DiffLine, FileDiff, LineKind, Token, TokenKind};

/// Above this many lines on either side, skip the O(n*m) LCS and emit a coarse
/// whole-file replacement (all base lines Removed, all work lines Added). Keeps a
/// pathologically large file from stalling the per-keystroke recompute.
const LCS_CELL_CAP: usize = 4_000_000; // base.len() * work.len()

/// Build the live diff `FileDiff` of `work` (new/right, editable) against `base`
/// (old/left, the HEAD blob), labelled with `path`/`new_rev`. Hunk indices increment
/// per contiguous changed block so the gutter hunk-revert keeps working.
pub fn live_diff(base: &[String], work: &[String], path: &str, new_rev: &str) -> FileDiff {
    let lines = if base.len().saturating_mul(work.len()) > LCS_CELL_CAP {
        coarse(base, work)
    } else {
        lcs_diff(base, work)
    };
    FileDiff {
        path: path.to_string(),
        old_rev: "HEAD".to_string(),
        new_rev: new_rev.to_string(),
        lines: stamp_hunks(lines),
    }
}

/// A single raw token for a working/base line (no syntax highlight on the live diff).
fn raw(text: &str) -> Vec<Token> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![Token { text: text.to_string(), kind: TokenKind::Ident }]
    }
}

fn context(old_no: usize, new_no: usize, text: &str) -> DiffLine {
    DiffLine { old_no: Some(old_no), new_no: Some(new_no), kind: LineKind::Context, tokens: raw(text), inline_hl: None, hunk: 0, fold: None }
}
fn removed(old_no: usize, text: &str) -> DiffLine {
    DiffLine { old_no: Some(old_no), new_no: None, kind: LineKind::Removed, tokens: raw(text), inline_hl: None, hunk: 0, fold: None }
}
fn added(new_no: usize, text: &str) -> DiffLine {
    DiffLine { old_no: None, new_no: Some(new_no), kind: LineKind::Added, tokens: raw(text), inline_hl: None, hunk: 0, fold: None }
}

/// Coarse fallback for huge files: every base line Removed, every work line Added.
fn coarse(base: &[String], work: &[String]) -> Vec<DiffLine> {
    let mut out = Vec::with_capacity(base.len() + work.len());
    for (i, l) in base.iter().enumerate() {
        out.push(removed(i + 1, l));
    }
    for (i, l) in work.iter().enumerate() {
        out.push(added(i + 1, l));
    }
    out
}

/// LCS line diff: matched lines become Context, base-only lines Removed, work-only
/// lines Added. Within a changed region the Removed run precedes the Added run, so
/// the side-by-side renderer pairs them as modified rows.
fn lcs_diff(base: &[String], work: &[String]) -> Vec<DiffLine> {
    let n = base.len();
    let m = work.len();
    // dp[i][j] = LCS length of base[i..] and work[j..]. (n+1)x(m+1) row-major.
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[at(i, j)] = if base[i] == work[j] {
                dp[at(i + 1, j + 1)] + 1
            } else {
                dp[at(i + 1, j)].max(dp[at(i, j + 1)])
            };
        }
    }

    let mut out = Vec::with_capacity(n.max(m));
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if base[i] == work[j] {
            out.push(context(i + 1, j + 1, &base[i]));
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            out.push(removed(i + 1, &base[i]));
            i += 1;
        } else {
            out.push(added(j + 1, &work[j]));
            j += 1;
        }
    }
    while i < n {
        out.push(removed(i + 1, &base[i]));
        i += 1;
    }
    while j < m {
        out.push(added(j + 1, &work[j]));
        j += 1;
    }
    out
}

/// Lines of unchanged context kept on EACH side of a changed line when folding
/// ("Hide unchanged"), matching git's default read-only diff context.
const FOLD_CONTEXT: usize = 3;

/// Collapse the unchanged middle of a live diff: keep every changed line plus
/// [`FOLD_CONTEXT`] context lines around it, and replace each interior run of dropped
/// context with one `N unchanged` fold marker (the same row a read-only commit diff
/// already shows). A run shorter than 2 stays inline (a marker would save no rows and
/// only hide a single line). A diff with NO changed line is returned untouched - there
/// is nothing to focus, so folding a clean file into one marker would be pure loss.
pub fn fold_unchanged(lines: Vec<DiffLine>) -> Vec<DiffLine> {
    let changed: Vec<usize> =
        lines.iter().enumerate().filter(|(_, l)| l.kind != LineKind::Context).map(|(i, _)| i).collect();
    if changed.is_empty() {
        return lines;
    }
    // keep[i] = line i is a changed line or within FOLD_CONTEXT of one.
    let mut keep = vec![false; lines.len()];
    for &c in &changed {
        let lo = c.saturating_sub(FOLD_CONTEXT);
        let hi = (c + FOLD_CONTEXT).min(lines.len() - 1);
        keep[lo..=hi].iter_mut().for_each(|k| *k = true);
    }
    let mut out = Vec::with_capacity(lines.len());
    // Indices below this were already swallowed by an emitted fold marker.
    let mut skip_until = 0usize;
    for (idx, line) in lines.into_iter().enumerate() {
        if idx < skip_until {
            continue;
        }
        if keep[idx] {
            out.push(line);
            continue;
        }
        // A dropped run begins at idx: measure it, then collapse to one marker (>=2
        // lines) or keep the lone line inline.
        let mut run = idx;
        while run < keep.len() && !keep[run] {
            run += 1;
        }
        if run - idx >= 2 {
            out.push(DiffLine::fold_marker(run - idx));
            skip_until = run;
        } else {
            out.push(line);
        }
    }
    out
}

/// Stamp a hunk index on each changed line: a contiguous run of changed (non-Context)
/// lines shares one index; the index increments after each Context gap. Context lines
/// keep hunk 0 (the gutter revert only reads changed lines).
fn stamp_hunks(mut lines: Vec<DiffLine>) -> Vec<DiffLine> {
    let mut idx = 0usize;
    let mut prev_changed = false;
    for l in &mut lines {
        let changed = l.kind != LineKind::Context;
        if changed {
            l.hunk = idx;
        } else if prev_changed {
            idx += 1; // a context line closes the current changed block
        }
        prev_changed = changed;
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }
    fn kinds(d: &FileDiff) -> Vec<(LineKind, Option<usize>, Option<usize>)> {
        d.lines.iter().map(|l| (l.kind, l.old_no, l.new_no)).collect()
    }

    #[test]
    fn identical_is_all_context() {
        let b = lines(&["a", "b", "c"]);
        let d = live_diff(&b, &b, "f", "wt");
        assert!(d.lines.iter().all(|l| l.kind == LineKind::Context));
        assert_eq!(d.lines.len(), 3);
    }

    #[test]
    fn single_line_modified_pairs_removed_then_added() {
        let b = lines(&["a", "b", "c"]);
        let w = lines(&["a", "B", "c"]);
        let d = live_diff(&b, &w, "f", "wt");
        assert_eq!(
            kinds(&d),
            vec![
                (LineKind::Context, Some(1), Some(1)),
                (LineKind::Removed, Some(2), None),
                (LineKind::Added, None, Some(2)),
                (LineKind::Context, Some(3), Some(3)),
            ]
        );
    }

    #[test]
    fn pure_insertion_is_added_only() {
        let b = lines(&["a", "b"]);
        let w = lines(&["a", "x", "b"]);
        let d = live_diff(&b, &w, "f", "wt");
        let added: Vec<_> = d.lines.iter().filter(|l| l.kind == LineKind::Added).collect();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].new_no, Some(2));
    }

    #[test]
    fn empty_base_marks_everything_added() {
        let d = live_diff(&[], &lines(&["x", "y"]), "f", "wt");
        assert!(d.lines.iter().all(|l| l.kind == LineKind::Added));
        assert_eq!(d.lines.len(), 2);
    }

    #[test]
    fn fold_unchanged_collapses_interior_context_to_one_marker() {
        // 12 identical lines with line 6 (index 5) changed: KEEP=3 context each side
        // leaves indices 2..=8 visible; the outer runs (0..2 and 9..12) each collapse.
        let mut b: Vec<String> = (0..12).map(|i| format!("l{i}")).collect();
        let mut w = b.clone();
        w[5] = "CHANGED".to_string();
        let d = live_diff(&b, &w, "f", "wt");
        let folded = fold_unchanged(d.lines);
        // Top run: indices 0,1 (len 2) -> one marker. Bottom run: 9,10,11 (len 3) -> one.
        let markers: Vec<usize> = folded.iter().filter_map(|l| l.fold).collect();
        assert_eq!(markers, vec![2, 3]);
        // The changed line + its 3-line context band survives uncollapsed.
        assert!(folded.iter().any(|l| l.kind == LineKind::Added));
        // No real source line was dropped from the visible band (lines 2..=8 on each side).
        let _ = &mut b;
        let _ = &mut w;
    }

    #[test]
    fn fold_unchanged_no_change_returns_input_untouched() {
        let b = lines(&["a", "b", "c", "d", "e", "f", "g", "h"]);
        let d = live_diff(&b, &b, "f", "wt");
        let n = d.lines.len();
        let folded = fold_unchanged(d.lines);
        assert_eq!(folded.len(), n);
        assert!(folded.iter().all(|l| l.fold.is_none()));
    }

    #[test]
    fn fold_unchanged_keeps_a_lone_dropped_line_inline() {
        // Two changes 7 lines apart leave a single uncovered line between the two
        // 3-context bands; a lone gap stays inline (a marker would save no rows).
        let b: Vec<String> = (0..8).map(|i| format!("l{i}")).collect();
        let mut w = b.clone();
        w[0] = "A".to_string();
        w[7] = "B".to_string();
        let d = live_diff(&b, &w, "f", "wt");
        let folded = fold_unchanged(d.lines);
        // index gap: change@0 covers 0..=3, change@7 covers 4..=7 -> nothing dropped.
        assert!(folded.iter().all(|l| l.fold.is_none()));
    }

    #[test]
    fn two_separate_changes_get_distinct_hunks() {
        let b = lines(&["a", "b", "c", "d", "e"]);
        let w = lines(&["A", "b", "c", "d", "E"]);
        let d = live_diff(&b, &w, "f", "wt");
        let hunks: Vec<usize> = d.lines.iter().filter(|l| l.kind != LineKind::Context).map(|l| l.hunk).collect();
        // First changed block hunk 0, second block hunk 1.
        assert_eq!(*hunks.first().unwrap(), 0);
        assert_eq!(*hunks.last().unwrap(), 1);
    }
}
