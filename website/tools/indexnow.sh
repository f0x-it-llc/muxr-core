#!/usr/bin/env bash
# Tell IndexNow search engines which muxr.app URLs to recrawl.
#
# Reads every <loc> from sitemap.xml and POSTs the list as JSON to
# https://api.indexnow.org/indexnow, with the key from indexnow-key.txt and
# keyLocation https://muxr.app/indexnow-key.txt (served from the site root).
#
# Sends nothing unless called with --send; without it the JSON payload is
# printed instead. Run it after a deploy:
#
#   tools/indexnow.sh           # print the payload
#   tools/indexnow.sh --send    # send it and print the HTTP status
set -euo pipefail

site="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
host="muxr.app"
endpoint="https://api.indexnow.org/indexnow"
key_location="https://muxr.app/indexnow-key.txt"

key="$(tr -d '[:space:]' < "$site/indexnow-key.txt")"
if ! [[ "$key" =~ ^[0-9a-f]{32}$ ]]; then
  echo "indexnow-key.txt must hold one 32-character lowercase hex key" >&2
  exit 1
fi

urls="$(sed -n 's|.*<loc>\(.*\)</loc>.*|\1|p' "$site/sitemap.xml")"
if [ -z "$urls" ]; then
  echo "no <loc> entries found in sitemap.xml" >&2
  exit 1
fi

url_json="$(printf '%s\n' "$urls" | sed 's|.*|    "&"|' | paste -sd '#' - | sed 's|#|,\
|g')"
payload="$(printf '{\n  "host": "%s",\n  "key": "%s",\n  "keyLocation": "%s",\n  "urlList": [\n%s\n  ]\n}\n' \
  "$host" "$key" "$key_location" "$url_json")"

if [ "${1:-}" != "--send" ]; then
  echo "Dry run: nothing sent. Re-run with --send to POST this payload to $endpoint"
  printf '%s' "$payload"
  exit 0
fi

status="$(printf '%s' "$payload" | curl -sS -o /dev/null -w '%{http_code}' \
  -X POST "$endpoint" -H 'Content-Type: application/json; charset=utf-8' --data-binary @-)"
echo "IndexNow responded HTTP $status"
