#!/usr/bin/env bash
# Watches a running frame for hours and says when it stops.
#
# READ-ONLY. It asks for /api/status and nothing else. This is the one script here meant to be
# pointed at the frame you actually use, with the album you actually care about, so it must never
# create, upload or delete anything. scripts/web-app-checks.sh is the one that mutates; keep them
# straight.
#
# The hang it is looking for does not announce itself: every route keeps answering, the process
# stays alive, and only the photos stop changing. So what is watched is the age of the last photo
# shown, plus how long each thread has been on one activity.
#
#   BASE        where the web app is      (default http://127.0.0.1:8080)
#   AUTH        user:password             (default frame:framepass)
#   HOURS       how long to watch         (default 12; 0 means until interrupted)
#   POLL        seconds between readings  (default 30)
#   MAX_STALE   seconds without a new photo before calling it stopped (default 600)
#   MAX_BUSY    seconds on one activity before calling a thread stuck (default 900)
#   LOG         where to write the readings (default ./soak-<date>.jsonl)
#   KEEP_GOING  set to 1 to carry on watching after the first stall instead of exiting
#
# MAX_STALE wants to be comfortably more than --interval: a frame on a ten-minute interval is not
# stalled at nine. The default suits intervals up to a few minutes.
set -uo pipefail

BASE="${BASE:-http://127.0.0.1:8080}"
AUTH="${AUTH:-frame:framepass}"
HOURS="${HOURS:-12}"
POLL="${POLL:-30}"
MAX_STALE="${MAX_STALE:-600}"
MAX_BUSY="${MAX_BUSY:-900}"
LOG="${LOG:-soak-$(date +%Y%m%d-%H%M%S).jsonl}"
KEEP_GOING="${KEEP_GOING:-0}"

started=$(date +%s)
deadline=0
if [ "$HOURS" != "0" ]; then
  deadline=$(( started + $(python3 -c "print(int(float('$HOURS') * 3600))") ))
fi

readings=0
unreachable=0
stalls=0

say() { echo "[$(date '+%H:%M:%S')] $*"; }

say "watching $BASE"
say "  a photo must appear at least every ${MAX_STALE}s, and no thread may sit on one activity for ${MAX_BUSY}s"
say "  readings go to $LOG"
[ "$HOURS" = "0" ] && say "  running until interrupted" || say "  running for ${HOURS}h"
echo

report_stall() { # why
  stalls=$((stalls + 1))
  echo
  say "STALLED: $1"
  say "the last few readings:"
  tail -5 "$LOG" | python3 -c '
import json, sys
for line in sys.stdin:
    r = json.loads(line)
    threads = ", ".join("%s=%s(%ss)" % (t["name"], t["doing"], t["for_seconds"])
                        for t in r.get("threads", []))
    shown = r.get("last_photo_shown_seconds_ago")
    print("  %s  last photo %s | %s" % (
        r["at"], "never" if shown is None else "%ss ago" % shown, threads))
'
  say "on the device, the frame's own log is the next place to look:"
  say "  journalctl -u syno-photo-frame --since '1 hour ago'"
  if [ "$KEEP_GOING" != "1" ]; then
    finish 1
  fi
}

finish() {
  echo
  say "watched for $(( ($(date +%s) - started) / 60 )) minutes, $readings readings"
  say "  unreachable: $unreachable"
  say "  stalls: $stalls"
  say "  readings kept in $LOG"
  exit "${1:-0}"
}
trap 'echo; say "interrupted"; finish 0' INT TERM

while true; do
  if [ "$deadline" != "0" ] && [ "$(date +%s)" -ge "$deadline" ]; then
    say "time is up"
    finish 0
  fi

  body=$(curl -s --max-time 20 -u "$AUTH" "$BASE/api/status" 2>/dev/null)
  if [ -z "$body" ]; then
    unreachable=$((unreachable + 1))
    say "no answer from $BASE (${unreachable} so far)"
    # Not treated as a stall: a frame that has died is visible in a way a wedged one is not, and a
    # flaky wifi link should not be reported as the bug being hunted.
    sleep "$POLL"
    continue
  fi

  verdict=$(echo "$body" | MAX_STALE="$MAX_STALE" MAX_BUSY="$MAX_BUSY" python3 -c '
import json, os, sys, datetime

status = json.load(sys.stdin)
status["at"] = datetime.datetime.now().strftime("%H:%M:%S")
max_stale = int(os.environ["MAX_STALE"])
max_busy = int(os.environ["MAX_BUSY"])

threads = {t["name"]: t for t in status.get("threads", [])}
display = threads.get("display", {})

# Legitimate reasons for no new photo, which must not be reported as the bug being hunted.
asleep = "asleep" in display.get("doing", "")
empty = bool(status.get("waiting_for_a_photo"))

reasons = []
shown = status.get("last_photo_shown_seconds_ago")
if not asleep and not empty:
    if shown is None:
        # Only after the frame has had time to show one; uptime covers the startup case.
        if status.get("uptime_seconds", 0) > max_stale:
            reasons.append("no photo has been shown at all in %ss of uptime"
                           % status["uptime_seconds"])
    elif shown > max_stale:
        reasons.append("the last photo was shown %ss ago" % shown)

for name, t in threads.items():
    # The web thread is always mid-request when it answers this one, so its age says nothing.
    if name == "web":
        continue
    if asleep and name == "display":
        continue
    if empty and name == "photo-fetcher":
        continue
    if t["for_seconds"] > max_busy:
        reasons.append("%s has been %s for %ss" % (name, t["doing"], t["for_seconds"]))

status["stalled"] = bool(reasons)
status["reasons"] = reasons
status["asleep"] = asleep
print(json.dumps(status))
')

  if [ -z "$verdict" ]; then
    say "could not read the status (the frame answered, but not with json)"
    sleep "$POLL"
    continue
  fi

  echo "$verdict" >> "$LOG"
  readings=$((readings + 1))

  if [ "$(echo "$verdict" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["stalled"]))')" = "1" ]; then
    report_stall "$(echo "$verdict" | python3 -c 'import json,sys; print("; ".join(json.load(sys.stdin)["reasons"]))')"
  elif [ $(( readings % 20 )) -eq 1 ]; then
    # A line now and then, so an unattended run shows it is still going
    say "$(echo "$verdict" | python3 -c '
import json, sys
s = json.load(sys.stdin)
shown = s.get("last_photo_shown_seconds_ago")
print("ok: last photo %s | %s" % (
    "never" if shown is None else "%ss ago" % shown,
    ", ".join("%s %ss" % (t["name"], t["for_seconds"]) for t in s.get("threads", []))))
')"
  fi

  sleep "$POLL"
done
