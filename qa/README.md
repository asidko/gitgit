# qa - real-screen QA harness

Drives the REAL `gitgit` TUI in a throwaway container: a virtual X screen (Xvfb), a
real terminal (`xterm`), real mouse + keyboard (`xdotool`), real-pixel screenshots
(`import`). Not a mock - the actual binary rendering and responding to input.

## Why this stack

- `gitgit` needs glibc <= 2.39, so the base is **ubuntu:24.04** (glibc 2.39).
- `xterm` with a TrueType font has full Unicode coverage (the bitmap `10x20` font
  mojibakes box-drawing under UTF-8). Cell size is MEASURED from the window geometry
  after launch, so `click <col> <row>` is exact for whatever font/size is used.
- The binary is a bind-MOUNTED volume, so `reload` hot-swaps a fresh build with no
  image rebuild and no container restart (it just stops the TUI to release the mmap,
  copies, relaunches).

## Use

    cargo build --release            # produce the binary qa mounts
    cd qa
    ./qactl.sh build                 # build the image (once)
    ./qactl.sh up                    # run container + launch gitgit (130x36)
    ./qactl.sh key Tab               # send keys (Tab, Down, ctrl+s, Escape, ...)
    ./qactl.sh type "hello"          # type literal text
    ./qactl.sh click 95 12           # left-click cell (col,row), 0-based
    ./qactl.sh dclick 95 12          # double-click (word select)
    ./qactl.sh tclick 95 12          # triple-click (whole-line select)
    ./qactl.sh sel 80 5 95 5         # press-drag-release to select text
    ./qactl.sh scroll 95 12 down 3   # mouse wheel (up|down, N notches)
    ./qactl.sh shot name             # screenshot -> ~/Downloads/shots/name.png
    ./qactl.sh status                # nonzero if the TUI has exited (crashed/quit)
    ./qactl.sh reload                # hot-swap a freshly built binary
    ./qactl.sh down                  # tear down

Every input verb waits for the frame to STABILISE before returning (it polls the
captured window until two grabs are identical) instead of sleeping a fixed time, so
it never races the async loader nor over-waits. `settle` is also exposed directly.

## What lives where

- This `qa/` dir (in the repo) holds everything: `Containerfile`, `scripts/`, `qactl.sh`.
- `/tmp/gitgit-qa/{bin,repo}` is EPHEMERAL working state, recreated by `up`.
- Screenshots land in `~/Downloads/shots/` (visible to both the agent and the user).
