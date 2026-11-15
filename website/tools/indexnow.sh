#!/usr/bin/env bash
# Tell IndexNow search engines which muxr.app URLs to recrawl.
#
# Reads every <loc> from sitemap.xml and POSTs the list as JSON to
# https://api.indexnow.org/indexnow, with the key from indexnow-key.txt and
# keyLocation https://muxr.app/indexnow-key.txt (served from the site root).
# python3 reads the sitemap with an XML parser and writes the payload with
# json.dumps, so a minified sitemap, XML entities or quotes cannot corrupt it.
#
# Sends nothing unless called with --send; without it the JSON payload is
# printed instead. Run it after a deploy:
#
#   tools/indexnow.sh           # print the payload
#   tools/indexnow.sh --send    # send it, print the HTTP status, and exit
#                               # non-zero unless the status is 200 or 202
#
# Sourcing the script defines its functions without running it.
set -euo pipefail

host="muxr.app"
endpoint="https://api.indexnow.org/indexnow"
key_location="https://muxr.app/indexnow-key.txt"

# build_payload SITEMAP KEY: print the JSON payload, or fail when the sitemap
# does not parse or lists no URL.
build_payload() {
  python3 - "$1" "$host" "$2" "$key_location" <<'PY'
import json
import sys
import xml.etree.ElementTree as ET

sitemap, host, key, key_location = sys.argv[1:5]
try:
    tree = ET.parse(sitemap)
except (ET.ParseError, OSError) as err:
    sys.exit("cannot read sitemap.xml: " + str(err))
urls = []
for node in tree.iter():
    if node.tag.split("}")[-1] == "loc" and (node.text or "").strip():
        urls.append(node.text.strip())
if not urls:
    sys.exit("no <loc> entries found in sitemap.xml")
payload = {"host": host, "key": key, "keyLocation": key_location,
           "urlList": urls}
print(json.dumps(payload, indent=2, ensure_ascii=False))
PY
}

# report_status CODE: print the HTTP status IndexNow answered with; succeed
# only for 200 (OK) and 202 (Accepted, key validation pending).
report_status() {
  echo "IndexNow responded HTTP $1"
  case "$1" in
    200|202) return 0 ;;
    *)
      echo "IndexNow did not accept the URL list (expected HTTP 200 or 202)" >&2
      return 1
      ;;
  esac
}

main() {
  local site key payload status
  site="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

  key="$(tr -d '[:space:]' < "$site/indexnow-key.txt")"
  if ! [[ "$key" =~ ^[0-9a-f]{32}$ ]]; then
    echo "indexnow-key.txt must hold one 32-character lowercase hex key" >&2
    return 1
  fi

  payload="$(build_payload "$site/sitemap.xml" "$key")"

  if [ "${1:-}" != "--send" ]; then
    echo "Dry run: nothing sent. Re-run with --send to POST this payload to $endpoint"
    printf '%s\n' "$payload"
    return 0
  fi

  if ! status="$(printf '%s' "$payload" | curl -sS --max-time 30 --retry 2 \
      -o /dev/null -w '%{http_code}' -X POST "$endpoint" \
      -H 'Content-Type: application/json; charset=utf-8' --data-binary @-)"; then
    echo "IndexNow request failed (HTTP ${status:-000})" >&2
    return 1
  fi
  report_status "$status"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
