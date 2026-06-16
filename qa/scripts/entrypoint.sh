#!/usr/bin/env bash
# Start the virtual X screen, then idle so the container stays up for `podman exec`.
set -e
: "${SCREEN:=1920x1080x24}"
rm -f /tmp/.X99-lock
Xvfb :99 -screen 0 "$SCREEN" -nolisten tcp >/qa/xvfb.log 2>&1 &
# Wait for the display to accept connections.
for _ in $(seq 1 50); do
  if xdpyinfo -display :99 >/dev/null 2>&1; then break; fi
  sleep 0.1
done
echo "Xvfb :99 ready ($SCREEN)"
exec sleep infinity
