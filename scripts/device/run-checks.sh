#!/usr/bin/env bash
# Runs on the device. Stands up a throwaway FTP server and a headless frame against it, runs the
# web app checks, and tears both down again.
#
# Deliberately isolated from a frame that may already be running on this device: its own album
# under a temporary directory, its own FTP port, its own web port, and it never touches the
# service. Nothing here is installed system-wide.
#
# Expects to be run from the payload directory the deploy script copied over.
set -uo pipefail

PAYLOAD="$(cd "$(dirname "$0")" && pwd)"
WORK="${WORK:-$HOME/.cache/photo-frame-device-test}"
FTP_PORT="${FTP_PORT:-2121}"
WEB_PORT="${WEB_PORT:-8099}"
WEB_USER=frame
WEB_PASSWORD=devicetest
FTP_USER=tester
FTP_PASSWORD=testerpass

say() { echo "[device] $*"; }

cleanup() {
  say "stopping the frame and the ftp server"
  [ -n "${FRAME_PID:-}" ] && kill "$FRAME_PID" 2>/dev/null
  [ -n "${FTPD_PID:-}" ] && kill "$FTPD_PID" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

say "preparing a throwaway album in $WORK"
rm -rf "$WORK"
mkdir -p "$WORK/album/Photos/holiday" "$WORK/album/Photos/empty"
cp "$PAYLOAD/fixtures/"*.jpeg "$WORK/album/Photos/" 2>/dev/null || true
cp "$PAYLOAD/fixtures/one.jpeg" "$WORK/album/Photos/holiday/beach.jpg"
echo "not a photo" > "$WORK/album/Photos/notes.txt"
ls -1 "$WORK/album/Photos" | sed 's/^/  /'

say "making sure pyftpdlib is available"
PY="$WORK/venv/bin/python"
if python3 -c 'import pyftpdlib' 2>/dev/null; then
  PY=python3
  say "  already installed system-wide"
else
  python3 -m venv "$WORK/venv" || {
    echo "[device] could not create a virtualenv. Install python3-venv, or install pyftpdlib" >&2
    exit 1
  }
  "$WORK/venv/bin/pip" install --quiet pyftpdlib || {
    echo "[device] could not install pyftpdlib. Is this device online?" >&2
    exit 1
  }
  say "  installed into $WORK/venv (left behind so the next run is quick)"
fi

say "starting the ftp server on port $FTP_PORT"
ALBUM_ROOT="$WORK/album" FTP_PORT="$FTP_PORT" \
  FTP_USER="$FTP_USER" FTP_PASSWORD="$FTP_PASSWORD" \
  "$PY" "$PAYLOAD/test-ftp-server.py" > "$WORK/ftpd.log" 2>&1 &
FTPD_PID=$!
for _ in $(seq 1 20); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$FTP_PORT") 2>/dev/null; then break; fi
  sleep 0.5
done
if ! kill -0 "$FTPD_PID" 2>/dev/null; then
  echo "[device] the ftp server died on startup:" >&2
  cat "$WORK/ftpd.log" >&2
  exit 1
fi

say "starting the frame headless on web port $WEB_PORT"
chmod +x "$PAYLOAD/syno-photo-frame"
SDL_VIDEODRIVER=dummy \
SYNO_PHOTO_FRAME_PASSWORD="$FTP_PASSWORD" \
SYNO_PHOTO_FRAME_WEB_PASSWORD="$WEB_PASSWORD" \
RUST_LOG="${RUST_LOG:-info}" \
  "$PAYLOAD/syno-photo-frame" "ftp://$FTP_USER@127.0.0.1:$FTP_PORT/Photos" \
  --web-bind "127.0.0.1:$WEB_PORT" --web-user "$WEB_USER" \
  --interval 5 --transition none --disable-update-check \
  > "$WORK/frame.log" 2>&1 &
FRAME_PID=$!

say "waiting for the web app"
ready=
for _ in $(seq 1 40); do
  if curl -s --max-time 3 -o /dev/null -u "$WEB_USER:$WEB_PASSWORD" \
      "http://127.0.0.1:$WEB_PORT/api/status"; then
    ready=yes
    break
  fi
  if ! kill -0 "$FRAME_PID" 2>/dev/null; then break; fi
  sleep 1
done
if [ -z "$ready" ]; then
  echo "[device] the frame never served the web app. Its log:" >&2
  sed 's/^/  /' "$WORK/frame.log" >&2
  exit 1
fi
say "  up"

echo
echo "================ web app checks ================"
BASE="http://127.0.0.1:$WEB_PORT" \
AUTH="$WEB_USER:$WEB_PASSWORD" \
ALBUM="$WORK/album/Photos" \
  bash "$PAYLOAD/web-app-checks.sh"
checks=$?

echo
echo "================ does the slideshow advance? ================"
# The one check that needs time rather than a request: a frame that is fetching and displaying
# moves last_photo_shown forward. --interval is 5s, so three intervals is a generous window.
status() { curl -s --max-time 5 -u "$WEB_USER:$WEB_PASSWORD" "http://127.0.0.1:$WEB_PORT/api/status"; }
shown_before=$(status | python3 -c 'import json,sys; print(json.load(sys.stdin).get("last_photo_shown_seconds_ago"))')
say "waiting 16s for the slideshow to move on"
sleep 16
after=$(status)
shown_after=$(echo "$after" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("last_photo_shown_seconds_ago"))')
advance=1
if [ "$shown_after" = "None" ]; then
  echo "FAIL  no photo has been shown at all (was $shown_before, now $shown_after)"
  advance=0
elif [ "$shown_after" -gt 15 ]; then
  echo "FAIL  the last photo was shown ${shown_after}s ago, so the slideshow has stopped"
  advance=0
else
  echo "PASS  a photo was shown ${shown_after}s ago, so the slideshow is advancing"
fi
echo "--- what each thread is doing now ---"
echo "$after" | python3 -c '
import json, sys
for t in json.load(sys.stdin)["threads"]:
    print("  %s: %s (for %ss)" % (t["name"], t["doing"], t["for_seconds"]))
'

echo
echo "================ result ================"
if [ "$checks" -eq 0 ] && [ "$advance" -eq 1 ]; then
  say "everything passed"
  exit 0
fi
say "something failed. The frame's log:"
tail -40 "$WORK/frame.log" | sed 's/^/  /'
exit 1
