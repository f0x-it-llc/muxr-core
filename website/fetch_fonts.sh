#!/usr/bin/env bash
# fetch_fonts.sh — populate website/fonts/ from the muxr.app font catalog.
#
# The font binaries are NOT committed to git (see fonts/.gitignore): they are
# hosted at https://muxr.app/fonts/ (served by this site's own nginx `/fonts/`
# location, cached as long-lived immutable), and this script pulls them into
# the Docker build context before an image build. The `fonts-v1` GitHub
# release that used to back this script is being retired — hosting has moved
# to muxr.app (GitHub's anonymous release-download edge refuses non-browser
# clients outright, which is why it moved) — but the release itself has NOT
# been deleted yet; deletion is a separate, pending step. Only `curl` and
# `python3` are required; no `gh` CLI.
#
# Source of truth for filenames + integrity is the LOCAL repo manifest
# (../fonts/manifest.json) — not a fetched copy — since the checkout already
# has it. Each manifest entry carries its own absolute `url`, which is the
# default download source per file. Set FONTS_BASE_URL to override that
# default for every file (disaster recovery / a hosting migration): when set
# and non-empty, each file is fetched from "$FONTS_BASE_URL/<basename>"
# instead of its manifest `url`. Either way, the resolved URL must be
# https:// — a plaintext source is refused per-file, mirroring the app's own
# download rule.
#
# Usage:
#   cd website && ./fetch_fonts.sh
#   docker build -t muxr-web .
#
# Idempotent: skips any file that already exists on disk with a matching
# sha256 (font files are immutable by contract under a given name), and
# (re-)downloads anything missing or mismatched. Downloads land in a
# <dest>.part temp file first and are only renamed into place after the
# sha256 verifies, so a curl failure, a stalled connection, or a hash
# mismatch never leaves a truncated or rejected file at the real path.
# curl is hardened against both hangs and TLS downgrade: pinned to
# https-only + TLS 1.2+, a 120s overall timeout, and 3 retries on transient
# connection failures. Per-file failures are collected and reported as a
# summary at the end (the loop does not abort on the first failure); after a
# successful verification pass, any *.ttf/*.otf on disk that is NOT listed in
# the manifest is pruned so stale binaries can never reach the image. The
# script exits non-zero if any file failed to fetch or verify.

set -euo pipefail
cd "$(dirname "$0")"

FONTS_BASE_URL="${FONTS_BASE_URL:-}"
MANIFEST="../fonts/manifest.json"

mkdir -p fonts

python3 - "$FONTS_BASE_URL" "$MANIFEST" << 'EOF'
import hashlib
import json
import os
import subprocess
import sys
from urllib.parse import urlsplit

base_url, manifest_path = sys.argv[1], sys.argv[2]
manifest = json.load(open(manifest_path))

def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()

bad = 0
known_names = set()
for entry in manifest["fonts"]:
    for f in entry["files"]:
        name = f["path"].split("/")[-1]
        known_names.add(name)
        dest = os.path.join("fonts", name)
        part = dest + ".part"
        want_sha = f["sha256"]

        if os.path.exists(dest) and sha256_of(dest) == want_sha:
            print(f"SKIP     {name} (already valid)")
            continue

        # Precedence: FONTS_BASE_URL override (per-run, applies to every
        # file) beats the manifest's own per-file url.
        url = f"{base_url}/{name}" if base_url else f["url"]

        if urlsplit(url).scheme != "https":
            print(f"REJECT   {name} <- {url} (refusing non-https source)", file=sys.stderr)
            bad += 1
            continue

        print(f"FETCH    {name} <- {url}")
        try:
            if os.path.exists(part):
                os.remove(part)
            subprocess.run(
                [
                    "curl", "-fsS",
                    "--proto", "=https",
                    "--tlsv1.2",
                    "--max-time", "120",
                    "--retry", "3",
                    "--retry-connrefused",
                    "-o", part, url,
                ],
                check=True,
            )
        except subprocess.CalledProcessError as exc:
            print(f"FAIL     {name} (curl exit {exc.returncode})", file=sys.stderr)
            if os.path.exists(part):
                os.remove(part)
            bad += 1
            continue

        actual_sha = sha256_of(part)
        if actual_sha != want_sha:
            print(f"MISMATCH {name}", file=sys.stderr)
            os.remove(part)
            bad += 1
            continue

        os.replace(part, dest)
        print(f"OK       {name}")

if bad:
    sys.exit(f"{bad} file(s) failed verification — not safe to build an image")

# Prune stale binaries: anything on disk that the manifest no longer lists
# must not reach the build context (or the image).
pruned = 0
for name in sorted(os.listdir("fonts")):
    if not (name.endswith(".ttf") or name.endswith(".otf")):
        continue
    if name in known_names:
        continue
    os.remove(os.path.join("fonts", name))
    print(f"PRUNE    {name} (not in manifest)")
    pruned += 1
if pruned:
    print(f"pruned {pruned} stale font file(s) not present in the manifest")

print("all font files verified against the manifest")
EOF
