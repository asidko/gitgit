#!/usr/bin/env bash
# In-container QA driver: runs the REAL gitgit TUI inside an xterm on the Xvfb
# display, and drives it with real X mouse/keyboard events + real screenshots.
#
# The xterm uses TrueType DejaVu Sans Mono at -fs 16 (the bitmap font mojibakes
# box-drawing under UTF-8), so the cell size is NOT fixed. It is MEASURED from the
# real window geometry after launch and a click at cell (col,row) (0-based) maps to
# the pixel center computed from the UN-TRUNCATED window width/height
# (col*WIDTH/cols + WIDTH/2/cols), so there is no per-column integer-division drift.
# The xterm window is pinned at the screen origin (+0+0).
#
# Readiness is STATE-based, not time-based: input verbs and `start` wait via
# `settle` (poll the captured frame until it stops changing) instead of a blind
# sleep, so they never race the async loader/repaint nor over-wait a fast frame.
#
#   start [W] [H]        launch gitgit in a fresh xterm of W x H cells
#   key   <keys...>      xdotool key (e.g. Tab, Down, ctrl+s, Escape, ctrl+c)
#   type  <string>       type literal characters
#   click <col> <row>    left-click at the 0-based cell
#   dclick <col> <row>   double-click (word select)
#   tclick <col> <row>   triple-click (whole-line select)
#   sel   <c1> <r1> <c2> <r2>   press, drag through a waypoint, release (selection)
#   scroll <col> <row> <up|down> [notches]   mouse wheel (4=up, 5=down)
#   hwheel <col> <row> <left|right> [notches]  NATIVE horizontal wheel (6=left, 7=right)
#   swheel <col> <row> <up|down> [notches]   shift+wheel (horizontal-scroll convention)
#   settle [max_polls] [idle_s]   block until the frame is stable (or polls exhausted)
#   shot  <name>         save /qa/shots/<name>.png (real pixels)
#   status               nonzero if the TUI has EXITED (crashed / quit)
#   stop                 close the xterm
set -uo pipefail
export DISPLAY=:99 LANG=C.UTF-8 LC_ALL=C.UTF-8
BIN=/qa/bin/gitgit
REPO=/qa/repo
SHOTS=/qa/shots
WIDF=/qa/.wid
GEOF=/qa/.geo      # measured geometry: "WIDTH HEIGHT COLS ROWS"

geo() { cat "$GEOF" 2>/dev/null || echo "1040 612 130 36"; }
# Pixel center of a cell, from the UN-TRUNCATED window size (no integer-division drift).
px() { local W H C R; read -r W H C R < <(geo); echo $(( $1 * W / C + W / (2 * C) )); }
py() { local W H C R; read -r W H C R < <(geo); echo $(( $1 * H / R + H / (2 * R) )); }
wid() { cat "$WIDF" 2>/dev/null; }
# Fail loud if there is no live window (start first / the TUI died) rather than
# feeding an empty id into px/py and silently mis-targeting every later action.
have_win() { [ -n "$(wid)" ] || { echo "qa: no xterm window (start first / TUI may have exited)" >&2; return 1; }; }
# Best-effort focus. windowactivate "errors" under a WM-less Xvfb but still routes
# input to the single pinned window; the meaningful failure (no window) is have_win's
# job, so this benign activate noise stays silenced.
focus() { local w; w=$(wid); [ -n "$w" ] && xdotool windowactivate --sync "$w" 2>/dev/null; }

# Block until the captured frame stops changing (two identical grabs idle_s apart),
# or max_polls is exhausted. This is the readiness oracle that replaces fixed sleeps.
settle() {
  local max="${1:-20}" idle="${2:-0.12}" w a=/tmp/.settle_a.png b=/tmp/.settle_b.png i
  w=$(wid); [ -n "$w" ] || return 0
  import -window "$w" "$a" 2>/dev/null || return 0
  for (( i = 0; i < max; i++ )); do
    sleep "$idle"
    import -window "$w" "$b" 2>/dev/null || return 0
    [ "$(compare -metric AE "$a" "$b" null: 2>&1)" = "0" ] && return 0
    mv -f "$b" "$a"
  done
  echo "qa: settle did not stabilize after $max polls (frame still changing)" >&2
  return 0   # non-fatal: the caller still captures; this only warns of a slow/animated frame
}

start() {
  local w="${1:-150}" h="${2:-40}"
  stop
  # TrueType DejaVu Mono (wide Unicode coverage incl box-drawing) + UTF-8 (-u8), no
  # internal border (-b 0), pinned at +0+0. The shell lingers (echo EXITED; sleep) so
  # the window survives if gitgit exits, keeping geometry stable for screenshots; the
  # EXITED sentinel is what `status` polls to detect a dead TUI.
  xterm -u8 -fa "DejaVu Sans Mono" -fs 16 -b 0 -geometry "${w}x${h}+0+0" \
        -e bash -lc "cd '$REPO' && '$BIN'; echo EXITED; sleep 600" \
        >/qa/xterm.log 2>&1 &
  local id
  id=$(xdotool search --sync --class XTerm | tail -1)
  [ -n "$id" ] || { echo "qa: xterm failed to launch (no window id)" >&2; cat /qa/xterm.log >&2; return 1; }
  echo "$id" > "$WIDF"
  xdotool windowmove "$id" 0 0 windowactivate --sync "$id"
  # Measure the cell size from the actual window geometry; store the full geometry so
  # px/py use the un-truncated width.
  eval "$(xdotool getwindowgeometry --shell "$id")"  # sets WIDTH, HEIGHT
  printf '%s %s %s %s\n' "$WIDTH" "$HEIGHT" "$w" "$h" > "$GEOF"
  settle 30 0.12   # wait for the loader's first stable frame instead of a blind sleep
  echo "started gitgit in xterm $id (${w}x${h} cells, ~$(( WIDTH / w ))x$(( HEIGHT / h )) px each)"
}

key()   { have_win || return 1; focus; xdotool key  --clearmodifiers "$@"; sleep 0.05; settle; }
type_() { have_win || return 1; focus; xdotool type --clearmodifiers -- "$1"; sleep 0.05; settle; }

click()  { have_win || return 1; focus; xdotool mousemove --sync "$(px "$1")" "$(py "$2")" click 1; sleep 0.05; settle; }

# Right-click at a cell (button 3): opens a context menu in the app under the pointer.
rclick() { have_win || return 1; focus; xdotool mousemove --sync "$(px "$1")" "$(py "$2")" click 3; sleep 0.05; settle; }

# Ctrl-click / Shift-click at a cell: the modifier is held around the left click (the app's
# multi-select gestures - toggle a mark / range-select). keydown/keyup fence the click.
cclick() { have_win || return 1; focus; xdotool mousemove --sync "$(px "$1")" "$(py "$2")" keydown ctrl click 1 keyup ctrl; sleep 0.05; settle; }
shclick() { have_win || return 1; focus; xdotool mousemove --sync "$(px "$1")" "$(py "$2")" keydown shift click 1 keyup shift; sleep 0.05; settle; }

# Double / triple click at a cell: N presses close enough in time to register as a
# multi-click (the app's window is 400ms; --delay is ms between presses).
dclick() { have_win || return 1; focus; xdotool mousemove --sync "$(px "$1")" "$(py "$2")" click --repeat 2 --delay 120 1; sleep 0.05; settle; }
tclick() { have_win || return 1; focus; xdotool mousemove --sync "$(px "$1")" "$(py "$2")" click --repeat 3 --delay 120 1; sleep 0.05; settle; }

# Press at (c1,r1), drag THROUGH the midpoint, release at (c2,r2). A real drag fires
# motion deltas; a one-shot teleport can under-report the selection the user gets.
sel() {
  have_win || return 1; focus
  local mx=$(( ($1 + $3) / 2 )) my=$(( ($2 + $4) / 2 ))
  xdotool mousemove --sync "$(px "$1")"  "$(py "$2")"  mousedown 1
  xdotool mousemove --sync "$(px "$mx")" "$(py "$my")"
  xdotool mousemove --sync "$(px "$3")"  "$(py "$4")"
  xdotool mouseup 1
  sleep 0.05; settle
}

# Mouse-wheel at a cell: buttons 4=up, 5=down. Each notch is one wheel event.
scroll() {
  have_win || return 1; focus
  local btn=5; [ "$3" = "up" ] && btn=4
  xdotool mousemove --sync "$(px "$1")" "$(py "$2")" click --repeat "${4:-1}" "$btn"
  sleep 0.05; settle
}

# NATIVE horizontal mouse-wheel at a cell: buttons 6=left, 7=right (xterm reports them
# as SGR scroll-left/right, which the app decodes as ScrollLeft/ScrollRight). This is the
# ONLY way to drive the diff's horizontal scroll from a real wheel; the vertical `scroll`
# verb cannot exercise it. Use to QA long-line read-only diffs (word-wrap off).
hwheel() {
  have_win || return 1; focus
  local btn=7; [ "$3" = "left" ] && btn=6
  xdotool mousemove --sync "$(px "$1")" "$(py "$2")" click --repeat "${4:-1}" "$btn"
  sleep 0.05; settle
}

# SHIFT + vertical wheel at a cell: the common terminal horizontal-scroll convention. Holds
# Shift down ACROSS the wheel clicks.
# LIMITATION: xterm RESERVES Shift+Btn4/5 for its OWN scrollback (default translations
# `Shift <Btn4Down>: scroll-back` / `Btn5Down: scroll-forw`), so it CONSUMES the event and
# never forwards it to the app - this verb scrolls xterm's scrollback, NOT the diff. The
# app's shift+wheel handler (map_mouse, ScrollDown/Up if SHIFT -> h_wheel) is real and works
# on terminals that forward it (kitty/alacritty/wezterm), but is UNDRIVABLE in this xterm
# harness. To test the diff's horizontal scroll here use `hwheel` (native btn 6/7, forwarded)
# or the keyboard hscroll (Diff pane focused: arrows/Home/End) - both reach the same path.
swheel() {
  have_win || return 1; focus
  local btn=5; [ "$3" = "up" ] && btn=4
  xdotool keydown shift
  xdotool mousemove --sync "$(px "$1")" "$(py "$2")" click --repeat "${4:-1}" "$btn"
  xdotool keyup shift
  sleep 0.05; settle
}

shot() {
  mkdir -p "$SHOTS"
  local w; w=$(wid)
  [ -n "$w" ] || { echo "qa: shot: no xterm window (start first)" >&2; return 1; }
  # Capture just the xterm window. A failed capture is an ERROR, not a silent
  # whole-root fallback that writes a misleading PNG the agent then has to diagnose.
  if ! import -window "$w" "$SHOTS/$1.png" 2>/qa/.import.err; then
    echo "qa: shot: import failed:" >&2; cat /qa/.import.err >&2; return 1
  fi
  echo "wrote $SHOTS/$1.png"
}

status() {
  if grep -q '^EXITED$' /qa/xterm.log 2>/dev/null; then
    echo "qa: TUI has EXITED (see /qa/xterm.log)" >&2; return 1
  fi
  echo "qa: TUI running (xterm $(wid))"
}

stop() {
  pkill -x xterm 2>/dev/null || true
  rm -f "$WIDF"
  # Wait until the old window is actually gone so start()'s search cannot latch a
  # dying window (which would target every later click/shot at the wrong window).
  local i
  for i in $(seq 1 20); do xdotool search --class XTerm >/dev/null 2>&1 || break; sleep 0.1; done
}

cmd="${1:-}"; shift || true
case "$cmd" in
  start)  start "$@" ;;
  key)    key "$@" ;;
  type)   type_ "$*" ;;
  click)  click "$@" ;;
  rclick) rclick "$@" ;;
  cclick) cclick "$@" ;;
  shclick) shclick "$@" ;;
  dclick) dclick "$@" ;;
  tclick) tclick "$@" ;;
  sel)    sel "$@" ;;
  scroll) scroll "$@" ;;
  hwheel) hwheel "$@" ;;
  swheel) swheel "$@" ;;
  settle) settle "$@" ;;
  shot)   shot "$@" ;;
  status) status ;;
  stop)   stop ;;
  *) echo "usage: qa.sh {start|key|type|click|rclick|cclick|shclick|dclick|tclick|sel|scroll|hwheel|swheel|settle|shot|status|stop}" >&2; exit 2 ;;
esac
