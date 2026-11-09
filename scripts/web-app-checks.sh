#!/usr/bin/env bash
# Checks the web app end to end against a running frame.
#
# Written against a *test* album whose contents this script knows, and it creates and deletes
# things, so never point it at an album you care about. It is the same set of checks whether the
# frame runs on this machine or on the device, which is the point: the device test is this script
# plus a way of getting onto the device.
#
#   BASE    where the web app is          (default http://127.0.0.1:8080)
#   AUTH    user:password for it          (default frame:framepass)
#   ALBUM   the album directory on disk, for checks that compare against the real files.
#           Leave unset when running against a frame whose files are not reachable from here;
#           those checks are then skipped rather than failing.
set -uo pipefail

BASE="${BASE:-http://127.0.0.1:8080}"
AUTH="${AUTH:-frame:framepass}"
ALBUM="${ALBUM:-}"
GUARD="X-Photo-Frame: 1"

pass=0
fail=0
skip=0

check() { # name expected actual
  if [ "$2" = "$3" ]; then
    echo "PASS  $1"
    pass=$((pass + 1))
  else
    echo "FAIL  $1 -- expected [$2] got [$3]"
    fail=$((fail + 1))
  fi
}

skipped() { # name why
  echo "SKIP  $1 ($2)"
  skip=$((skip + 1))
}

code() { curl -s --max-time 20 -o /dev/null -w '%{http_code}' "$@"; }
encode() { python3 -c "import sys,urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=''))" "$1"; }

echo "--- reachable at all ---"
if ! curl -s --max-time 20 -o /dev/null -u "$AUTH" "$BASE/api/status"; then
  echo "FAIL  the web app did not answer at $BASE"
  echo
  echo "passed 0, failed 1, skipped 0"
  exit 1
fi
echo "PASS  the web app answers at $BASE"
pass=$((pass + 1))

echo "--- auth and guards ---"
check "no credentials is 401" 401 "$(code "$BASE/api/list?path=")"
check "wrong password is 401" 401 "$(code -u "${AUTH%%:*}:definitelywrong" "$BASE/api/list?path=")"
check "credentials get the page" 200 "$(code -u "$AUTH" "$BASE/")"
check "mutating route without the guard header is 403" \
  403 "$(code -u "$AUTH" -X POST "$BASE/api/mkdir?path=&name=x")"
check "foreign Host is 421" \
  421 "$(code -u "$AUTH" -H "Host: photos.example.com:8080" "$BASE/api/list?path=")"
check "a .local Host is allowed" \
  200 "$(code -u "$AUTH" -H "Host: frame.local:${BASE##*:}" "$BASE/api/list?path=")"
check "right host, wrong port is 421" \
  421 "$(code -u "$AUTH" -H "Host: 127.0.0.1:9999" "$BASE/api/list?path=")"

echo "--- status ---"
STATUS=$(curl -s --max-time 20 -u "$AUTH" "$BASE/api/status")
check "status reports the photo fetcher" 1 \
  "$(echo "$STATUS" | python3 -c 'import json,sys; print(int(any(t["name"]=="photo-fetcher" for t in json.load(sys.stdin)["threads"])))')"
check "status reports an age for every thread" 1 \
  "$(echo "$STATUS" | python3 -c 'import json,sys; print(int(all(isinstance(t["for_seconds"],int) for t in json.load(sys.stdin)["threads"])))')"
check "status reports a version" 1 \
  "$(echo "$STATUS" | python3 -c 'import json,sys; print(int(bool(json.load(sys.stdin)["version"])))')"

echo "--- listing ---"
LIST=$(curl -s --max-time 20 -u "$AUTH" "$BASE/api/list?path=")
check "the root listing is valid json with photos and directories" 1 \
  "$(echo "$LIST" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(int("photos" in d and "directories" in d))')"
check "a non-photo is not listed" 0 "$(echo "$LIST" | grep -c 'notes.txt')"
FIRST_PHOTO=$(echo "$LIST" | python3 -c 'import json,sys; p=json.load(sys.stdin)["photos"]; print(p[0]["name"] if p else "")')
if [ -z "$FIRST_PHOTO" ]; then
  skipped "image checks" "the album has no photos"
else
  echo "--- images (using $FIRST_PHOTO) ---"
  check "thumbnail is a jpeg" "image/jpeg" \
    "$(curl -s --max-time 30 -o /dev/null -w '%{content_type}' -u "$AUTH" \
       "$BASE/api/thumbnail?path=$(encode "$FIRST_PHOTO")")"
  check "the full-size photo is served" 200 \
    "$(code -u "$AUTH" "$BASE/api/photo?path=$(encode "$FIRST_PHOTO")")"
  check "the thumbnail is smaller than the photo" "smaller" \
    "$( [ "$(curl -s --max-time 30 -u "$AUTH" "$BASE/api/thumbnail?path=$(encode "$FIRST_PHOTO")" | wc -c)" -lt \
         "$(curl -s --max-time 30 -u "$AUTH" "$BASE/api/photo?path=$(encode "$FIRST_PHOTO")" | wc -c)" ] \
       && echo smaller || echo "not smaller")"
fi

echo "--- path safety ---"
check "parent traversal is rejected" 400 "$(code -u "$AUTH" "$BASE/api/list?path=..%2F..%2Fetc")"
check "double-encoded traversal stays a literal name" 502 \
  "$(code -u "$AUTH" "$BASE/api/list?path=%252e%252e%252f")"
check "an absolute path is rejected" 400 "$(code -u "$AUTH" "$BASE/api/list?path=%2Fetc")"
check "a non-photo extension is rejected" 400 "$(code -u "$AUTH" "$BASE/api/photo?path=notes.txt")"

echo "--- mutations ---"
PROBE="probe-$$"
check "mkdir creates a folder" 201 \
  "$(code -u "$AUTH" -H "$GUARD" -X POST "$BASE/api/mkdir?path=&name=$PROBE")"
check "the new folder appears in the listing" 1 \
  "$(curl -s --max-time 20 -u "$AUTH" "$BASE/api/list?path=" \
     | python3 -c "import json,sys; print(int(any(d['name']=='$PROBE' for d in json.load(sys.stdin)['directories'])))")"

if [ -n "$FIRST_PHOTO" ]; then
  curl -s --max-time 60 -u "$AUTH" "$BASE/api/photo?path=$(encode "$FIRST_PHOTO")" -o /tmp/upload-source.jpg
  check "upload stores a photo" 201 \
    "$(code -u "$AUTH" -H "$GUARD" --data-binary "@/tmp/upload-source.jpg" \
       -X POST "$BASE/api/upload?path=$PROBE&name=uploaded.jpg")"
  check "the uploaded photo is listed" 1 \
    "$(curl -s --max-time 20 -u "$AUTH" "$BASE/api/list?path=$PROBE" \
       | python3 -c 'import json,sys; print(int(len(json.load(sys.stdin)["photos"]) == 1))')"
  NAME=$(curl -s --max-time 60 -u "$AUTH" -H "$GUARD" --data-binary "@/tmp/upload-source.jpg" \
    -X POST "$BASE/api/upload?path=$PROBE&name=uploaded.jpg")
  check "a second upload of the same name is renamed, not overwritten" 1 \
    "$(echo "$NAME" | grep -c -v '"uploaded.jpg"')"
  if [ -n "$ALBUM" ]; then
    check "the uploaded bytes match what was sent" "same" \
      "$(cmp -s "$ALBUM/$PROBE/uploaded.jpg" /tmp/upload-source.jpg && echo same || echo different)"
  else
    skipped "the uploaded bytes match what was sent" "ALBUM not set"
  fi
  check "rmdir refuses a non-empty folder" 409 \
    "$(code -u "$AUTH" -H "$GUARD" -X POST "$BASE/api/rmdir?path=$PROBE")"
  # read, not $(...): the second upload is renamed to something like "uploaded (2).jpg", and word
  # splitting on that leaves two halves that delete nothing and a folder that will not go away
  curl -s --max-time 20 -u "$AUTH" "$BASE/api/list?path=$PROBE" \
    | python3 -c 'import json,sys; [print(p["name"]) for p in json.load(sys.stdin)["photos"]]' \
    | while IFS= read -r name; do
        [ -n "$name" ] || continue
        curl -s --max-time 30 -o /dev/null -u "$AUTH" -H "$GUARD" -X POST \
          "$BASE/api/delete?path=$PROBE%2F$(encode "$name")"
      done
  check "the folder is empty after deleting" 0 \
    "$(curl -s --max-time 20 -u "$AUTH" "$BASE/api/list?path=$PROBE" \
       | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["photos"]))')"
else
  skipped "upload and delete checks" "no photo to copy"
fi

check "rmdir refuses the share root" 400 \
  "$(code -u "$AUTH" -H "$GUARD" -X POST "$BASE/api/rmdir?path=")"
check "rmdir removes the empty folder" 204 \
  "$(code -u "$AUTH" -H "$GUARD" -X POST "$BASE/api/rmdir?path=$PROBE")"
check "the folder is gone from the listing" 0 \
  "$(curl -s --max-time 20 -u "$AUTH" "$BASE/api/list?path=" \
     | python3 -c "import json,sys; print(int(any(d['name']=='$PROBE' for d in json.load(sys.stdin)['directories'])))")"

echo
echo "passed $pass, failed $fail, skipped $skip"
[ "$fail" -eq 0 ]
