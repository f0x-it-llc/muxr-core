#!/usr/bin/env bash
# gen_manifest.sh — regenerate manifest.json from the fonts/ directory.
#
# Usage:
#   cd muxr-core/fonts && ./gen_manifest.sh
#
# The script is re-runnable and deterministic.  It:
#   1. Scans *.ttf AND *.otf files in this directory.
#   2. Computes exact byte sizes (stat) and sha256 hashes (sha256sum).
#   3. Derives weight/style from the filename suffix (-Regular→400/normal, -Bold→700/normal).
#   4. Groups files into font families using a hardcoded metadata table (keyed by filename prefix),
#      because family, displayName, license, and isNerdFont cannot be reliably derived from the
#      filename alone.
#   5. Writes manifest.json.
#
# To add a new font:
#   1. Drop the Regular + Bold .ttf (or .otf) files into this directory.
#   2. Add a row to the FONT_META table below.
#   3. Run ./gen_manifest.sh.
#
# Fail-closed on missing binaries: every family in the metadata table below
# must match at least one file on disk, or the script errors out and exits 1
# instead of silently writing a manifest.json missing that family's files —
# a truncated manifest committed by accident is a supply-chain incident, not
# a cosmetic bug. To intentionally regenerate a partial manifest (e.g. only
# a subset of binaries are checked out locally), set SKIP_MISSING=1 to
# downgrade the error to a loud warning and omit the family instead.

set -euo pipefail
cd "$(dirname "$0")"

# ─── Hosting ────────────────────────────────────────────────────────────────
# Font binaries are NOT committed in-tree; they are hosted at
# https://muxr.app/fonts/ (the website's own nginx `/fonts/` location, cached
# as long-lived immutable) and each entry's absolute "url" points there.
# Override the base with FONTS_BASE_URL=... if the hosting location ever
# changes.
FONTS_BASE_URL="${FONTS_BASE_URL:-https://muxr.app/fonts}"

# ─── Metadata table ──────────────────────────────────────────────────────────
# Columns (tab-separated):
#   file_prefix   id                      displayName                      family                             license      isNerdFont
#
# file_prefix  = the part before -Regular.ttf/.otf / -Bold.ttf/.otf (exact filename stem minus weight suffix)
# family       = internal PostScript/name-table family string (MUST match what Flutter's FontLoader registers)
#                Verify with: fc-scan --format '%{family}\n' <file.ttf|.otf>
# isNerdFont   = "true" or "false"
#
declare -A META_ID META_DISPLAY META_FAMILY META_LICENSE META_ISNF
declare -a ALL_PREFIXES

add_meta() {
    local prefix="$1" id="$2" display="$3" family="$4" license="$5" isnf="$6"
    META_ID["$prefix"]="$id"
    META_DISPLAY["$prefix"]="$display"
    META_FAMILY["$prefix"]="$family"
    META_LICENSE["$prefix"]="$license"
    META_ISNF["$prefix"]="$isnf"
    ALL_PREFIXES+=("$prefix")
}

# ── Original catalog ──────────────────────────────────────────────────────────

# Plain JetBrains Mono (the app's bundled default — also available for download)
add_meta "JetBrainsMono" \
         "jetbrains-mono" \
         "JetBrains Mono" \
         "JetBrains Mono" \
         "OFL-1.1" \
         "false"

# JetBrains Mono Nerd Font Mono
add_meta "JetBrainsMonoNerdFontMono" \
         "jetbrains-mono-nf" \
         "JetBrains Mono Nerd Font Mono" \
         "JetBrainsMono Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Fira Code Nerd Font Mono
add_meta "FiraCodeNerdFontMono" \
         "fira-code-nf" \
         "Fira Code Nerd Font Mono" \
         "FiraCode Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Hack Nerd Font Mono
add_meta "HackNerdFontMono" \
         "hack-nf" \
         "Hack Nerd Font Mono" \
         "Hack Nerd Font Mono" \
         "MIT" \
         "true"

# Cascadia Code → CaskaydiaCove Nerd Font Mono
add_meta "CaskaydiaCoveNerdFontMono" \
         "cascadia-code-nf" \
         "Cascadia Code Nerd Font Mono" \
         "CaskaydiaCove Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Iosevka Term Nerd Font Mono
add_meta "IosevkaTermNerdFontMono" \
         "iosevka-term-nf" \
         "Iosevka Term Nerd Font Mono" \
         "IosevkaTerm Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Meslo LG S Nerd Font Mono
add_meta "MesloLGSNerdFontMono" \
         "meslo-lgs-nf" \
         "Meslo LG S Nerd Font Mono" \
         "MesloLGS Nerd Font Mono" \
         "Apache-2.0" \
         "true"

# Source Code Pro → SauceCodePro Nerd Font Mono
add_meta "SauceCodeProNerdFontMono" \
         "source-code-pro-nf" \
         "Source Code Pro Nerd Font Mono" \
         "SauceCodePro Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Ubuntu Mono Nerd Font Mono
add_meta "UbuntuMonoNerdFontMono" \
         "ubuntu-mono-nf" \
         "Ubuntu Mono Nerd Font Mono" \
         "UbuntuMono Nerd Font Mono" \
         "UFL-1.0" \
         "true"

# Inconsolata Nerd Font Mono
add_meta "InconsolataNerdFontMono" \
         "inconsolata-nf" \
         "Inconsolata Nerd Font Mono" \
         "Inconsolata Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Roboto Mono Nerd Font Mono
add_meta "RobotoMonoNerdFontMono" \
         "roboto-mono-nf" \
         "Roboto Mono Nerd Font Mono" \
         "RobotoMono Nerd Font Mono" \
         "Apache-2.0" \
         "true"

# ── Expanded catalog (wave 2) ─────────────────────────────────────────────────

# Monaspace Neon (Ne) — OTF superfamily; 5 separate entries
add_meta "MonaspiceNeNerdFontMono" \
         "monaspace-neon-nf" \
         "Monaspace Neon Nerd Font Mono" \
         "MonaspiceNe Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Monaspace Argon (Ar)
add_meta "MonaspiceArNerdFontMono" \
         "monaspace-argon-nf" \
         "Monaspace Argon Nerd Font Mono" \
         "MonaspiceAr Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Monaspace Xenon (Xe)
add_meta "MonaspiceXeNerdFontMono" \
         "monaspace-xenon-nf" \
         "Monaspace Xenon Nerd Font Mono" \
         "MonaspiceXe Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Monaspace Radon (Rn)
add_meta "MonaspiceRnNerdFontMono" \
         "monaspace-radon-nf" \
         "Monaspace Radon Nerd Font Mono" \
         "MonaspiceRn Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Monaspace Krypton (Kr)
add_meta "MonaspiceKrNerdFontMono" \
         "monaspace-krypton-nf" \
         "Monaspace Krypton Nerd Font Mono" \
         "MonaspiceKr Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# CommitMono Nerd Font Mono — OTF
add_meta "CommitMonoNerdFontMono" \
         "commit-mono-nf" \
         "Commit Mono Nerd Font Mono" \
         "CommitMono Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Geist Mono Nerd Font Mono — OTF
add_meta "GeistMonoNerdFontMono" \
         "geist-mono-nf" \
         "Geist Mono Nerd Font Mono" \
         "GeistMono Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Victor Mono Nerd Font Mono — TTF
add_meta "VictorMonoNerdFontMono" \
         "victor-mono-nf" \
         "Victor Mono Nerd Font Mono" \
         "VictorMono Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# IBM Plex Mono → BlexMono Nerd Font Mono — TTF
add_meta "BlexMonoNerdFontMono" \
         "ibm-plex-mono-nf" \
         "IBM Plex Mono Nerd Font Mono" \
         "BlexMono Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Hasklig → Hasklug Nerd Font Mono — OTF
add_meta "HasklugNerdFontMono" \
         "hasklig-nf" \
         "Hasklig Nerd Font Mono" \
         "Hasklug Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# 0xProto Nerd Font Mono — TTF
add_meta "0xProtoNerdFontMono" \
         "0xproto-nf" \
         "0xProto Nerd Font Mono" \
         "0xProto Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# IntelOne Mono → IntoneMono Nerd Font Mono — TTF
add_meta "IntoneMonoNerdFontMono" \
         "intel-one-mono-nf" \
         "Intel One Mono Nerd Font Mono" \
         "IntoneMono Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Lilex Nerd Font Mono — TTF
add_meta "LilexNerdFontMono" \
         "lilex-nf" \
         "Lilex Nerd Font Mono" \
         "Lilex Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# Space Mono Nerd Font Mono — TTF
add_meta "SpaceMonoNerdFontMono" \
         "space-mono-nf" \
         "Space Mono Nerd Font Mono" \
         "SpaceMono Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# DejaVu Sans Mono → DejaVuSansM Nerd Font Mono — TTF
add_meta "DejaVuSansMNerdFontMono" \
         "dejavu-sans-mono-nf" \
         "DejaVu Sans Mono Nerd Font Mono" \
         "DejaVuSansM Nerd Font Mono" \
         "custom" \
         "true"

# Cousine Nerd Font Mono — TTF
add_meta "CousineNerdFontMono" \
         "cousine-nf" \
         "Cousine Nerd Font Mono" \
         "Cousine Nerd Font Mono" \
         "Apache-2.0" \
         "true"

# BigBlueTerminal 437 Nerd Font Mono — TTF (no Bold)
add_meta "BigBlueTerm437NerdFontMono" \
         "bigblue-terminal-437-nf" \
         "BigBlue Terminal 437 Nerd Font Mono" \
         "BigBlueTerm437 Nerd Font Mono" \
         "CC-BY-SA-4.0" \
         "true"

# BigBlueTerminal Plus Nerd Font Mono — TTF (no Bold)
add_meta "BigBlueTermPlusNerdFontMono" \
         "bigblue-terminal-plus-nf" \
         "BigBlue Terminal Plus Nerd Font Mono" \
         "BigBlueTermPlus Nerd Font Mono" \
         "CC-BY-SA-4.0" \
         "true"

# Terminus → Terminess Nerd Font Mono — TTF
add_meta "TerminessNerdFontMono" \
         "terminus-nf" \
         "Terminus Nerd Font Mono" \
         "Terminess Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# ProFont IIx Nerd Font Mono — TTF (no Bold)
add_meta "ProFontIIxNerdFontMono" \
         "profont-iix-nf" \
         "ProFont IIx Nerd Font Mono" \
         "ProFont IIx Nerd Font Mono" \
         "MIT" \
         "true"

# ProFont Windows Nerd Font Mono — TTF (no Bold)
add_meta "ProFontWindowsNerdFontMono" \
         "profont-windows-nf" \
         "ProFont Windows Nerd Font Mono" \
         "ProFontWindows Nerd Font Mono" \
         "MIT" \
         "true"

# Go Mono Nerd Font Mono — TTF
add_meta "GoMonoNerdFontMono" \
         "go-mono-nf" \
         "Go Mono Nerd Font Mono" \
         "GoMono Nerd Font Mono" \
         "BSD-3-Clause" \
         "true"

# D2Coding Ligature Nerd Font Mono — TTF
add_meta "D2CodingLigatureNerdFontMono" \
         "d2coding-ligature-nf" \
         "D2Coding Ligature Nerd Font Mono" \
         "D2CodingLigature Nerd Font Mono" \
         "OFL-1.1" \
         "true"

# ─── Helpers ─────────────────────────────────────────────────────────────────

file_bytes() { stat -c '%s' "$1"; }
file_sha256() { sha256sum "$1" | cut -d' ' -f1; }

# Given a filename like FiraCodeNerdFontMono-Regular.ttf (or .otf) → "FiraCodeNerdFontMono"
file_prefix() {
    local base="${1%.ttf}"
    base="${base%.otf}"
    # Remove trailing -Regular, -Bold (and italic variants we don't track)
    echo "${base%-Regular}" | sed 's/-Bold$//'
}

# -Regular → weight=400,style="normal"
# -Bold    → weight=700,style="normal"
file_weight_style() {
    case "$1" in
        *-Regular.ttf|*-Regular.otf) echo "400 normal" ;;
        *-Bold.ttf|*-Bold.otf)       echo "700 normal" ;;
        *)                            echo "400 normal" ;;
    esac
}

# ─── Build manifest ──────────────────────────────────────────────────────────

# Collect fonts grouped by prefix, preserving insertion order for determinism.
declare -A SEEN_PREFIXES
declare -a PREFIX_ORDER

for font in $(ls *.ttf *.otf 2>/dev/null | sort); do
    prefix=$(file_prefix "$font")
    if [[ -z "${META_ID[$prefix]+_}" ]]; then
        echo "WARNING: no metadata for prefix '$prefix' (file: $font) — skipping" >&2
        continue
    fi
    if [[ -z "${SEEN_PREFIXES[$prefix]+_}" ]]; then
        SEEN_PREFIXES["$prefix"]=1
        PREFIX_ORDER+=("$prefix")
    fi
done

# ─── Guard: every known family must have matching binaries on disk ──────────
# The inverse check (files with no metadata) already warns above; this one
# catches a family that's registered but has ZERO files on disk, which the
# scan above would otherwise skip silently — the family just wouldn't appear
# in the emitted manifest.json at all.
missing=0
for prefix in "${ALL_PREFIXES[@]}"; do
    if [[ -z "${SEEN_PREFIXES[$prefix]+_}" ]]; then
        if [[ "${SKIP_MISSING:-0}" == "1" ]]; then
            echo "WARNING: no files on disk for '${META_DISPLAY[$prefix]}' (prefix '$prefix') — SKIP_MISSING=1, omitting from manifest.json. The result is a SUBSET manifest: do NOT commit it, and do NOT run fetch_fonts.sh with PRUNE=1 against it — that would delete the omitted binaries from website/fonts/, which may be their only local copy." >&2
        else
            echo "ERROR: no files on disk for '${META_DISPLAY[$prefix]}' (prefix '$prefix') — refusing to write a truncated manifest.json. Set SKIP_MISSING=1 to intentionally regenerate a subset." >&2
            missing=1
        fi
    fi
done
if [[ "$missing" == "1" ]]; then
    exit 1
fi

# Emit JSON
{
printf '{\n'
printf '  "version": 1,\n'
printf '  "fonts": [\n'

first_family=true
for prefix in "${PREFIX_ORDER[@]}"; do
    id="${META_ID[$prefix]}"
    display="${META_DISPLAY[$prefix]}"
    family="${META_FAMILY[$prefix]}"
    license="${META_LICENSE[$prefix]}"
    isnf="${META_ISNF[$prefix]}"

    if [[ "$first_family" == "true" ]]; then
        first_family=false
    else
        printf ',\n'
    fi

    printf '    {\n'
    printf '      "id": "%s",\n' "$id"
    printf '      "displayName": "%s",\n' "$display"
    printf '      "family": "%s",\n' "$family"
    printf '      "license": "%s",\n' "$license"
    printf '      "isNerdFont": %s,\n' "$isnf"
    printf '      "files": [\n'

    first_file=true
    for font in $(ls ${prefix}-Regular.ttf ${prefix}-Bold.ttf ${prefix}-Regular.otf ${prefix}-Bold.otf 2>/dev/null | sort); do
        [[ -f "$font" ]] || continue
        ws=($(file_weight_style "$font"))
        weight="${ws[0]}"
        style="${ws[1]}"
        bytes=$(file_bytes "$font")
        sha=$(file_sha256 "$font")

        if [[ "$first_file" == "true" ]]; then
            first_file=false
        else
            printf ',\n'
        fi

        printf '        { "weight": %s, "style": "%s", "path": "%s", "bytes": %s, "sha256": "%s", "url": "%s" }' \
               "$weight" "$style" "$font" "$bytes" "$sha" "$FONTS_BASE_URL/$font"
    done
    printf '\n'
    printf '      ]\n'
    printf '    }'
done

printf '\n'
printf '  ]\n'
printf '}\n'
} > manifest.json

echo "manifest.json written ($(python3 -c "import json;d=json.load(open('manifest.json'));print(len(d['fonts']),'families,',sum(len(f['files']) for f in d['fonts']),'files')" 2>/dev/null || echo 'parse check: run python3 manually'))"
