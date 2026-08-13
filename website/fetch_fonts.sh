#!/usr/bin/env bash
# fetch_fonts.sh — populate website/fonts/ from the muxr.app font catalog.
#
# The font binaries are NOT committed to git (see fonts/.gitignore): they are
# hosted at https://muxr.app/fonts/ (served by this site's own nginx `/fonts/`
# location, cached as long-lived immutable), and this script pulls them into
# the Docker build context before an image build. There is no GitHub release
# involved anymore — the `fonts-v1` release assets that used to back this
# script are gone (GitHub's ANONYMOUS release-download edge refuses
# non-browser clients outright — bot mitigation, connection reset without
# response — which is the reason the fonts moved to muxr.app in the first
# place). Only `curl` and `python3` are required; no `gh` CLI.
#
# Source of truth for filenames + integrity is the LOCAL repo manifest
# (../fonts/manifest.json) — not a fetched copy — since the checkout already
# has it. Override the download base with FONTS_BASE_URL (default
# https://muxr.app/fonts) for CI or a future hosting migration.
#
# Usage:
#   cd website && ./fetch_fonts.sh
#   docker build -t muxr-web .
#
# Idempotent: skips any file that already exists on disk with a matching
# sha256 (font files are immutable by contract under a given name), and
# (re-)downloads anything missing or mismatched. Every file — skipped or
# freshly downloaded — is hard-verified against the manifest's sha256 before
# the script exits 0, so a bad or partial download can never reach an image.

set -euo pipefail
cd "$(dirname "$0")"

FONTS_BASE_URL="${FONTS_BASE_URL:-https://muxr.app/fonts}"
MANIFEST="../fonts/manifest.json"

mkdir -p fonts

python3 - "$FONTS_BASE_URL" "$MANIFEST" << 'EOF'
import hashlib
import json
import os
import subprocess
import sys

base_url, manifest_path = sys.argv[1], sys.argv[2]
manifest = json.load(open(manifest_path))

def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()

bad = 0
for entry in manifest["fonts"]:
    for f in entry["files"]:
        name = f["path"].split("/")[-1]
        dest = os.path.join("fonts", name)
        want_sha = f["sha256"]

        if os.path.exists(dest) and sha256_of(dest) == want_sha:
            print(f"SKIP     {name} (already valid)")
            continue

        url = f"{base_url}/{name}"
        print(f"FETCH    {name} <- {url}")
        subprocess.run(
            ["curl", "-fsS", "-o", dest, url],
            check=True,
        )

        actual_sha = sha256_of(dest)
        if actual_sha != want_sha:
            print(f"MISMATCH {name}", file=sys.stderr)
            bad += 1
            continue
        print(f"OK       {name}")

if bad:
    sys.exit(f"{bad} file(s) failed verification — not safe to build an image")
print("all font files verified against the manifest")
EOF
