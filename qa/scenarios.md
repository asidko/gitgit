# QA scenario matrix - drive every feature in the REAL harness

Purpose: stop shipping "verified" features that are broken. Unit tests are necessary,
never sufficient. A feature is VERIFIED only when its exact user steps are driven through
`qactl.sh` and the resulting screenshot is read adversarially (you wrote the expected cells
BEFORE looking, then confirmed those pixels). Four pin/overlay regressions shipped green-on-
tests; every one would have died the first time it was driven here.

Read this WITH `../CLAUDE.md` "QA verification discipline". This file adds the concrete
geometry + command sequences so a driver does not re-derive them (and mis-click) every time.

## How to think (QA-lead mindset)

For each feature ask: what does the user CLICK / SCROLL / TYPE / ENTER, in what STATE, and
what do they EXPECT to see. Then drive exactly that. Two axes that catch real bugs:

- Bug-CLASS over feature-list. The expensive bugs are not "feature X is missing"; they are
  "state Y survives a navigation it should not". Hunt the class once, it covers many features.
- ENTRY x STATE matrix. A feature has N entry points (header button, menu item, key accel)
  crossed with M states (`<current>` vs historical commit, changed vs unchanged file, path
  absent at target, after commit-nav, after file-nav, after filter/search change). Drive
  every reachable cell, not just the one you designed.

## Harness geometry (130x36 cells, ~13x27 px; full window 1690x972)

Cell center in pixels = (col*13 + 6, row*27 + 13). Default layout, no `state.toml` in the
container, so these are stable:

- Menu bar = row 0. DIFF viewer = rows 1-18. hsep = row 19.
- BOTTOM pane: toolbar = row 20, hsep = row 21, panes = rows 22-35.
- `split_log_h = 0.62`: LOG pane = cols 0-79, divider = col 80, FILES+DETAIL = cols 81-129.
- Right column splits VERTICALLY: FILES pane on top (files header row 22, root `repo/`
  row 23, FIRST file row 24), DETAIL pane below.

Anchors on the fixture (`<current>` row selected at boot):

- `config.toml` = the only changed file = col ~85, ROW 24. Right-click -> full files menu.
- Files menu (15 rows, clamps to rows ~21-35; CROP to confirm): Show Current Revision ~24,
  Compare with Revision ~25, Compare with Branch or Tag ~26, Show History ~27, Annotate ~28,
  Pin Left ~30, Pin Right ~31, Rollback ~33, Delete ~34.
- Files toolbar (row 20): search focus = click col 88; clear `x` = col ~101; `.*` ~col 104;
  `Flat` ~col 112; `All` ~col 119.
- LOG commits (click col ~10): `<current>` 22, seed lib 23, commit idea dir 24, ...,
  `chore: bump config, env, gitignore` (sha dd346a64, TOUCHED config.toml) = row 31.
- Top menu bar: `Editor` ~col 1, `View` ~col 9, `Git` ~col 16.

Read a region precisely (do NOT eyeball the downscaled full shot - that is how clicks drift):
`magick ~/Downloads/shots/<shot>.png -crop 600x520+1090+560 +repage -resize 180% /tmp/c.png`
then Read /tmp/c.png.

QUIRK: while an inspect overlay is OPEN, character `type` is SWALLOWED (keys route to the
overlay). To re-land the files-search WITH an overlay open, use the MOUSE `x` clear
(col ~101 row 20), not typing. Set a search query BEFORE opening the overlay.

## Bug-CLASS 1: state that survives navigation must refresh at EVERY choke point

The choke points (store.rs): `select_commit`, `select_file`, `move_cursor_to`,
`after_filter_change`, `after_files_search_change`, `RepoLoaded` (--watch / post-write),
`toggle_flat`, `toggle_dir`. The parked state: inspect overlay, diff `pin`,
`edit_scroll`/`log_scroll`/`files_scroll`/`diff_hscroll` free-scroll, `notice_sticky`,
`diff_sel`/`detail_sel`. Matrix: each parked field x each choke point must be reset /
refreshed / released, or it freezes. Drive, do not read the code and assume.

PIN (the reference): pin a file (files menu Pin Left/Right, or the header ring), then drive
EACH choke point and confirm the title `pin X vs Y - Esc to unpin` stays on COMMIT-nav (other
side sweeps, pinned side byte-stable) and DROPS on FILE-nav / files-search change / mark-jump.

OVERLAYS (Show Current Revision / Compare with Revision / Compare with Branch / Annotate
Blame): open each, then click another COMMIT, another FILE, and clear the files-search (mouse
`x`) - after each, the overlay must dismiss/refresh, NEVER stay frozen on the old file while
the selection moved. Show History is special: confirming a picked revision NAVIGATES the log
and re-selects the file by path (not a parked overlay).

## Named regression checks (re-run every release; proven command sequences)

1. Read-only hscroll: select a commit, open `src/notes.go` (has a LONGLINE), then
   `hwheel <col> <row> right` over the diff body -> the line tail reveals, the gutter stays
   fixed. ALSO drive the keyboard path (`key End` / arrows with the Diff pane focused).
   swheel CAVEAT: `swheel` is INERT in this harness - xterm reserves Shift+Btn4/5 for its own
   scrollback and never forwards it to the app. The app's shift+wheel handler is real (works
   on kitty/alacritty/wezterm) but UNDRIVABLE in xterm; verify hscroll via hwheel + keyboard,
   which reach the same `h_wheel`/`ScrollDiffH` path. Do NOT file "swheel does nothing" as an
   app bug - it is a terminal limitation (see the swheel comment in scripts/qa.sh).
2. Notice in ONE place: trigger a Copy/commit notice -> it renders ONLY on the LOG filter
   toolbar (right-aligned), NEVER also on the files toolbar.
3. Unchanged file = ONE full-width pane: pick a commit, open an unchanged file -> single
   gutter, no empty right half, no same-text-on-both-sides.
4. Read-only text selection: drag (`sel`) over the committed diff -> bands the dragged chars;
   Ctrl+C copies them; a bare click leaves no band.
5. PIN holds across COMMIT-nav (title visible on a two-pane diff, other side sweeps) and
   RELEASES on FILE-nav; Esc unpins. PLUS: releases on files-search `x` clear and on a
   mark/range cursor-jump (the two choke points fixed 2026-06-07).

## Per-feature smoke (open + cancel; never confirm a destructive action in the fixture)

Do NOT confirm anything that mutates `/tmp/gitgit-qa/repo` HEAD (commit/amend/tag/push/
rollback/reset/delete/discard) - open the dialog, screenshot, Esc. Reseed with
`./qactl.sh repo` if a mutation happened.

- Files menu (file row): Show Diff / Copy as Patch / Create Patch / Commit File / Show Current
  Revision / Compare with Revision / Compare with Branch / Show History / Annotate / Pin
  Left|Right / Rollback / Delete. Gating: working `<current>` row = full menu; historical
  commit's file row = read-only inspect group only; unchanged file = Pin Left|Right only.
- Folder menu (dir row, `<current>` only): Commit Directory / Copy as Patch / Create Patch /
  Rollback. Root menu (`repo/` row): Copy Path / Zip Project. NEEDS a changed file UNDER a
  subdir: the seed dirties `cmd/main.go`, so after `./qactl.sh repo` the files pane shows
  `cmd/` (dir row 24) whose right-click opens this menu. A STALE container (seeded before that
  working change - only `config.toml` dirty) has no dir row; reseed first. (`up` does NOT
  reseed - see lessons.md.)
- Commit-log menu (real commit): Cherry-pick / Revert / Reset to here / Undo (HEAD only) /
  Reword / New Branch / New Tag / Create Patch / Export as patch / interactive-rebase mark
  items. Gating: working row hints instead of acting.
- Git top menu: Commit / Amend / Tag (input dialogs) / Fetch / Pull / Push (confirm) / Stash /
  Unstash / Discard. After a real commit/amend/tag/pull the repo reloads (log + `<current>`).
- Toolbars: search-history popup (magnifier lens), Branch/User filter dropdowns (cap + scroll),
  Flat/All toggle, focus-reveal bullseye.
- Editor menu: Undo / Redo / Autosave / Revert. View menu: Show Diff / Side by side / Word wrap
  / Whitespace.

## Wrap + scroll are first-class dimensions

- WRAP: re-run the wrapping panes (detail, diff side-by-side + unified) at a DELIBERATELY
  NARROW width; screenshot the wrapped rows; confirm continuation rows keep their indent (no
  fall to col 0), padding symmetric, full-width band rectangular across the fold, no glyph
  clipped at the right edge.
- SCROLL: wheel every scrollable pane (editable diff, log, files, detail, dropdowns);
  confirm the viewport moves while the cursor/selection stays put, the scrolled content is
  correct, a click after scrolling lands on the row drawn, and typing snaps the cursor back.

## The discipline (non-negotiable)

- ZERO harness drive = UNVERIFIED. Before writing "verified", `grep -rn <feature> qa/` must
  show the driven sequence + named shots.
- Read each shot adversarially: write the cell + expected content BEFORE looking, then confirm
  those pixels. "Looks fine" is decoration.
- Harness can't drive the input? Add the verb or write `UNVERIFIED: <why>`. Never substitute a
  unit test and call it verified.
- Final pass is end-to-end from COLD start (`up` fresh), every reported bug driven in order.
