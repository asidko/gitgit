#!/bin/sh
# gitgit installer - downloads the matching prebuilt binary from the latest
# GitHub release. Linux and macOS, x86_64 and arm64.
#
#   curl -fsSL https://raw.githubusercontent.com/asidko/gitgit/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/asidko/gitgit/main/install.sh | sh -s -- --remove
#
# Install dir defaults to ~/.local/bin (override with GITGIT_INSTALL_DIR).
set -eu

REPO="asidko/gitgit"
BIN="gitgit"
INSTALL_DIR="${GITGIT_INSTALL_DIR:-$HOME/.local/bin}"

err() { echo "gitgit: $*" >&2; exit 1; }

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Linux)  os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *) err "unsupported OS: $os (Linux and macOS only)" ;;
  esac
  case "$arch" in
    x86_64|amd64)   arch_part="x86_64" ;;
    aarch64|arm64)  arch_part="aarch64" ;;
    *) err "unsupported architecture: $arch" ;;
  esac
  echo "${arch_part}-${os_part}"
}

do_remove() {
  if [ -f "$INSTALL_DIR/$BIN" ]; then
    rm -f "$INSTALL_DIR/$BIN"
    echo "Removed $INSTALL_DIR/$BIN"
  else
    echo "$BIN is not installed in $INSTALL_DIR"
  fi
}

do_install() {
  command -v curl >/dev/null 2>&1 || err "curl is required"
  command -v tar  >/dev/null 2>&1 || err "tar is required"
  target="$(detect_target)"
  asset="gitgit-${target}.tar.gz"
  url="https://github.com/${REPO}/releases/latest/download/${asset}"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  echo "Downloading ${asset} ..."
  curl -fsSL "$url" -o "$tmp/$asset" || err "download failed: $url"
  tar -xzf "$tmp/$asset" -C "$tmp" || err "extract failed"
  [ -f "$tmp/$BIN" ] || err "archive did not contain $BIN"
  mkdir -p "$INSTALL_DIR"
  cp "$tmp/$BIN" "$INSTALL_DIR/$BIN"
  chmod 755 "$INSTALL_DIR/$BIN"
  echo "Installed $BIN -> $INSTALL_DIR/$BIN"
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "Note: $INSTALL_DIR is not on your PATH - add it to use 'gitgit'." ;;
  esac
}

case "${1:-}" in
  --remove|remove|uninstall) do_remove ;;
  ""|--install|install)      do_install ;;
  *) err "usage: install.sh [--remove]" ;;
esac
