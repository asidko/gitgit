#!/usr/bin/env bash
# Host control for the gitgit QA container: a REAL virtual X screen (Xvfb) running
# the actual TUI in xterm, driven by xdotool (real mouse + keyboard) and captured
# with ImageMagick `import` (real pixels).
#
#   ./qactl.sh build          build the image from this dir
#   ./qactl.sh up [W] [H]      run the container + launch gitgit (default 130x36)
#   ./qactl.sh reload [W] [H]  hot-swap the freshly-built binary + relaunch (no rebuild)
#   ./qactl.sh repo            (re)create the demo repo
#   ./qactl.sh down            stop + remove the container
#   ./qactl.sh <cmd> ...       proxy to the in-container driver:
#                              start|key|type|click|sel|shot|stop
#
# This script lives in the repo (survives /tmp wipes). It rebuilds EVERYTHING from
# the repo: the only state in /tmp is the ephemeral bin/ + repo/ working dirs, which
# `up` recreates automatically. Screenshots go to ~/Downloads/shots (agent + user see
# them). The binary is a VOLUME, so `reload` swaps it without an image rebuild.
set -u
SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"   # repo/qa (build context)
REPO_ROOT="$(cd "$SELF/.." && pwd)"
IMG=gitgit-qa
CT=gitgit-qa
SRC="$REPO_ROOT/target/release/gitgit"
WORK=/tmp/gitgit-qa                 # ephemeral: bin/ + repo/ (auto-recreated)
SHOTS="$HOME/Downloads/shots"

build() { podman build -t "$IMG" "$SELF"; }

repo() {
  rm -rf "$WORK/repo"; mkdir -p "$WORK/repo"; ( cd "$WORK/repo"
    git init -q; git config user.name "Ada Lovelace"; git config user.email "ada@example.com"
    git symbolic-ref HEAD refs/heads/main 2>/dev/null || true
    mkdir -p src cmd
    # Back-dated commits exercise the Today/Yesterday log dates (newest = now).
    d3="$(date -d '3 days ago' '+%Y-%m-%dT%H:%M:%S')"; d2="$(date -d '2 days ago' '+%Y-%m-%dT%H:%M:%S')"
    d1="$(date -d 'yesterday 14:05' '+%Y-%m-%dT%H:%M:%S')"; d0="$(date '+%Y-%m-%dT%H:%M:%S')"
    # Committed files across many languages (highlight coverage) incl. dotfiles.
    printf '# orbit\n\nTiny task scheduler. See **docs** and `code`.\n\n- one\n- two\n' > README.md
    printf 'node_modules\n*.log\n/dist\n' > .gitignore
    printf 'APP_ENV=dev\nPORT=8080\n# comment\n' > .env
    printf '[server]\nport = 8080\nname = "orbit"\n\n[log]\nlevel = "info"\n' > config.toml
    printf 'package orbit\n\n// Scheduler runs queued jobs.\ntype Scheduler struct {\n\tqueue []Job\n}\n\nfunc (s *Scheduler) Add(j Job) {\n\ts.queue = append(s.queue, j)\n}\n' > src/lib.go
    GIT_AUTHOR_DATE="$d3" GIT_COMMITTER_DATE="$d3" git add -A
    GIT_AUTHOR_DATE="$d3" GIT_COMMITTER_DATE="$d3" git commit -q -m "feat: initial scheduler skeleton"
    printf 'package orbit\n\ntype Job struct {\n\tName string\n\tRun  func() error\n}\n' > src/job.go
    GIT_AUTHOR_DATE="$d2" GIT_COMMITTER_DATE="$d2" git add -A
    GIT_AUTHOR_DATE="$d2" GIT_COMMITTER_DATE="$d2" git commit -q -m "feat: add Job type"
    printf 'package main\n\nimport "fmt"\n\nfunc main() {\n\tfmt.Println("orbit starting")\n}\n' > cmd/main.go
    printf 'package orbit\n\nimport "errors"\n\n// Scheduler runs queued jobs in order.\ntype Scheduler struct {\n\tqueue []Job\n}\n\nfunc (s *Scheduler) Add(j Job) {\n\ts.queue = append(s.queue, j)\n}\n\nfunc (s *Scheduler) RunAll() error {\n\tfor _, j := range s.queue {\n\t\tif err := j.Run(); err != nil {\n\t\t\treturn errors.New("job failed: " + j.Name)\n\t\t}\n\t}\n\treturn nil\n}\n' > src/lib.go
    # A COMMITTED file with a deliberately very long single line so a READ-ONLY picked
    # commit has a horizontally-scrollable diff (Bug 2 regression check via hwheel/swheel);
    # survives every re-seed so the case is reproducible by design, not hand-added per run.
    printf 'package orbit\n\n// LONGLINE: the scheduler dispatch table maps every known job name to its handler so a single very wide source line overflows the diff pane and forces horizontal scrolling to read its tail end here.\nvar dispatch = map[string]func() error{"alpha": nil, "bravo": nil, "charlie": nil, "delta": nil, "echo": nil, "foxtrot": nil}\n' > src/notes.go
    GIT_AUTHOR_DATE="$d1" GIT_COMMITTER_DATE="$d1" git add -A
    GIT_AUTHOR_DATE="$d1" GIT_COMMITTER_DATE="$d1" git commit -q -m "feat: cmd entrypoint and RunAll"
    # HEAD touches ONLY root files (v1, differs from base) so the per-commit tree lists
    # root FILES with NO leading directory (row 0 is a file -> startup auto-selects a
    # file -> exercises bug 3); those files have working-tree edits below (editable diff).
    printf 'node_modules\n*.log\n/dist\n/tmp\n' > .gitignore
    printf 'APP_ENV=staging\nPORT=8080\n# comment\n' > .env
    printf '[server]\nport = 8080\nname = "orbit"\nworkers = 2\n\n[log]\nlevel = "info"\n' > config.toml
    printf '# orbit\n\nTiny task scheduler. See **docs**.\n\n- one\n- two\n' > README.md
    # FORCE-add gitignored content (a *.log file + a whole ignored node_modules/ tree,
    # nested) so the All-files view can exercise dimming tracked-but-gitignored files.
    mkdir -p node_modules/leftpad/lib
    printf 'startup noise\n' > debug.log
    printf 'module.exports = function(){}\n' > node_modules/leftpad/index.js
    printf 'module.exports = 42\n' > node_modules/leftpad/lib/deep.js
    git add -f debug.log node_modules
    GIT_AUTHOR_DATE="$d0" GIT_COMMITTER_DATE="$d0" git add -A
    GIT_AUTHOR_DATE="$d0" GIT_COMMITTER_DATE="$d0" git commit -q -m "chore: bump config, env, gitignore"
    # A DIVERGENT side branch whose tip is NOT an ancestor of main and touches its OWN
    # file, so a clean cherry-pick / revert is drivable through the UI (the diff has no
    # overlap with main, so it applies without a conflict). Visible in the log via the
    # branch tip; survives every re-seed so the case is reproducible by design.
    git branch sidecar "$(git rev-parse HEAD~2)"   # off "feat: add Job type"
    git checkout -q sidecar
    printf 'package orbit\n\n// Sidecar adds an out-of-band metrics probe.\nfunc Probe() string {\n\treturn "ok"\n}\n' > src/sidecar.go
    GIT_AUTHOR_DATE="$d1" GIT_COMMITTER_DATE="$d1" git add -A
    GIT_AUTHOR_DATE="$d1" GIT_COMMITTER_DATE="$d1" git commit -q -m "feat: sidecar metrics probe"
    git checkout -q main
    # A remote-tracking ref so the two NEWEST commits read as UNPUSHED (hollow graph
    # node) while the older two are pushed (solid); no real remote URL needed.
    git update-ref refs/remotes/origin/main "$(git rev-parse HEAD~2)"
    # Many branches (spread across commits) so the Branch filter dropdown exceeds its
    # row cap and must scroll.
    for i in 1 2 3 4 5 6 7 8 9 10; do git branch "feature/topic-$i" "HEAD~$((i % 4))" 2>/dev/null || true; done
    # Two tags exercise the tag-locality lozenge: v1.0 on a PUSHED commit (HEAD~2 ==
    # origin/main) renders the FILLED lozenge; v2.0-rc1 on an UNPUSHED commit (HEAD~1)
    # renders the hollow lozenge. Annotated vs lightweight also exercises the tag-peel.
    git tag -a v1.0 "$(git rev-parse HEAD~2)" -m "release 1.0" 2>/dev/null || true
    git tag v2.0-rc1 "$(git rev-parse HEAD~1)" 2>/dev/null || true
    # Uncommitted working changes (many files vs HEAD) so the live editable diff has
    # real content and there are several files to multi-select + revert. README has a
    # very long line to exercise word-wrap; touches dotfiles/toml/md for highlighting.
    printf 'node_modules\n*.log\n/dist\n/coverage\n.env.local\n' > .gitignore
    printf 'APP_ENV=production\nPORT=9090\n# comment\nDEBUG=false\n' > .env
    printf '[server]\nport = 9090\nname = "orbit"\nworkers = 4\n\n[log]\nlevel = "debug"\n' > config.toml
    printf '# orbit\n\nTiny task scheduler. See **docs** and `code` plus a [link](https://example.test) that makes this paragraph long enough to wrap across several rows in a narrow side-by-side pane so word-wrap is clearly exercised end to end.\n\n- one\n- two\n- three\n' > README.md
    printf 'package main\n\nimport (\n\t"fmt"\n\t"os"\n)\n\nfunc main() {\n\tfmt.Println("orbit starting")\n\tif len(os.Args) > 1 {\n\t\tfmt.Println("config:", os.Args[1])\n\t}\n}\n' > cmd/main.go
  )
  echo "demo repo ready: $WORK/repo"
}

stage_bin() {
  [ -f "$SRC" ] || { echo "missing binary: $SRC (run: cargo build --release)" >&2; exit 1; }
  mkdir -p "$WORK/bin"; cp "$SRC" "$WORK/bin/gitgit"
}

up() {
  podman rm -f "$CT" >/dev/null 2>&1 || true   # frees the binary (mmap'd while running)
  stage_bin
  [ -d "$WORK/repo/.git" ] || repo
  mkdir -p "$SHOTS"
  podman run -d --name "$CT" \
    -v "$WORK/bin:/qa/bin:z" \
    -v "$SHOTS:/qa/shots:z" \
    -v "$WORK/repo:/qa/repo:z" \
    "$IMG" >/dev/null
  sleep 1.5
  podman exec "$CT" /qa/qa.sh start "${1:-130}" "${2:-36}"
}

# Hot-swap the binary: stop the TUI (releasing the mmap), copy the fresh build, relaunch.
reload() {
  podman exec "$CT" /qa/qa.sh stop || true
  stage_bin
  podman exec "$CT" /qa/qa.sh start "${1:-130}" "${2:-36}"
}

down() { podman rm -f "$CT" >/dev/null 2>&1 || true; echo "removed $CT"; }

cmd="${1:-}"; shift || true
case "$cmd" in
  build) build ;;
  repo)  repo ;;
  up)    up "$@" ;;
  reload) reload "$@" ;;
  down)  down ;;
  start|key|type|click|rclick|cclick|shclick|dclick|tclick|sel|scroll|hwheel|swheel|settle|shot|status|stop) podman exec "$CT" /qa/qa.sh "$cmd" "$@" ;;
  *) echo "usage: qactl.sh {build|repo|up|reload|down|start|key|type|click|rclick|cclick|shclick|dclick|tclick|sel|scroll|hwheel|swheel|settle|shot|status|stop}" >&2; exit 2 ;;
esac
