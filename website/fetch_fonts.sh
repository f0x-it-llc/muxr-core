#!/usr/bin/env bash
# fetch_fonts.sh — populate website/fonts/ from the muxr-core fonts release.
#
# The font binaries are NOT committed to git (see .gitignore): they live as
# assets on the muxr-core `fonts-v1` GitHub release, and this script pulls
# them into the Docker build context before an image build. It uses the
# authenticated `gh` CLI on purpose — GitHub's ANONYMOUS release-download
# edge (`github.com/…/releases/download/…`) refuses non-browser clients
# outright (bot mitigation, connection reset without response; wire-verified
# 2026-08-13, the reason the app stopped downloading from it and these fonts
# moved to https://muxr.app/fonts/ in the first place). Authenticated API
# downloads are not subject to that edge.
#
# Usage:
#   cd website && ./fetch_fonts.sh
#   docker build -t muxr-web .
#
# Idempotent: re-downloads with --clobber, then verifies every file against
# the sha256 the muxr-core manifest claims — a mismatch fails the script, so
# a bad or partial download can never reach an image.

set -euo pipefail
cd "$(dirname "$0")"

RELEASE_TAG="${FONTS_RELEASE_TAG:-fonts-v1}"
REPO="f0x-it-llc/muxr-core"
MANIFEST_URL="https://raw.githubusercontent.com/${REPO}/main/fonts/manifest.json"

mkdir -p fonts
gh release download "$RELEASE_TAG" -R "$REPO" -D fonts --clobber

curl -fsS "$MANIFEST_URL" -o fonts/.manifest.verify.json
python3 - << 'EOF'
import json, hashlib, os, sys
manifest = json.load(open('fonts/.manifest.verify.json'))
bad = 0
for entry in manifest['fonts']:
    for f in entry['files']:
        name = f['path'].split('/')[-1]
        path = os.path.join('fonts', name)
        if not os.path.exists(path):
            print(f'MISSING  {name}', file=sys.stderr); bad += 1; continue
        actual = hashlib.sha256(open(path, 'rb').read()).hexdigest()
        if actual != f['sha256']:
            print(f'MISMATCH {name}', file=sys.stderr); bad += 1
if bad:
    sys.exit(f'{bad} file(s) failed verification — not safe to build an image')
print('all font files verified against the manifest')
EOF
rm fonts/.manifest.verify.json
