# Muxr Font Catalog

This directory contains downloadable terminal fonts for the Muxr app.

The app fetches `manifest.json` at startup to discover available fonts, then
downloads individual `.ttf` files on demand when the user selects a font in
Settings. Files are cached locally on the device after the first download.

---

## URL Pattern

The app fetches files from the raw GitHub URL of this directory on the `main` branch:

```
https://raw.githubusercontent.com/f0x-it-llc/muxr-core/main/fonts/manifest.json
https://raw.githubusercontent.com/f0x-it-llc/muxr-core/main/fonts/<path>
```

where `<path>` is the `path` field from `manifest.json` (relative to this directory).

---

## manifest.json Schema

```jsonc
{
  "version": 1,
  "fonts": [
    {
      "id": "fira-code-nf",           // stable kebab-case identifier
      "displayName": "Fira Code Nerd Font Mono",
      "family": "FiraCode Nerd Font Mono",   // MUST equal the font's internal family name
      "license": "OFL-1.1",
      "isNerdFont": true,             // true = file contains Nerd Font icon glyphs
      "files": [
        {
          "weight": 400,              // CSS font-weight integer
          "style": "normal",          // "normal" | "italic"
          "path": "FiraCodeNerdFontMono-Regular.ttf",
          "bytes": 2647492,           // exact file size — verified by the app
          "sha256": "<lowercase hex>", // SHA-256 of the file — verified by the app
          "url": "https://muxr.app/fonts/FiraCodeNerdFontMono-Regular.ttf" // absolute download URL
        },
        { "weight": 700, "style": "normal", "path": "FiraCodeNerdFontMono-Bold.ttf", ... }
      ]
    }
  ]
}
```

`url` is hosted at `https://muxr.app/fonts/` (this website's own nginx `/fonts/` location,
cached as long-lived immutable). The `fonts-v1` GitHub release that used to back this is
being retired — hosting has moved to muxr.app — but the release has NOT been deleted yet;
deletion is a separate, pending step. `gen_manifest.sh` builds each entry's `url` from
`FONTS_BASE_URL` (default `https://muxr.app/fonts`); see "Regenerating manifest.json" below.

**Important:** `family` must be the font's internal PostScript/name-table family string —
the exact string Flutter's `FontLoader` registers. Verify it with:

```bash
fc-scan --format '%{family}\n' <file.ttf>
```

Getting this wrong means the app registers the font under the wrong name and cannot
use it via `TextStyle(fontFamily: ...)`.

---

## Included Font Families

### Original catalog

| ID | Display Name | Family (internal) | License | Nerd Font |
|----|-------------|-------------------|---------|-----------|
| `jetbrains-mono` | JetBrains Mono | `JetBrains Mono` | OFL-1.1 | No |
| `jetbrains-mono-nf` | JetBrains Mono Nerd Font Mono | `JetBrainsMono Nerd Font Mono` | OFL-1.1 | Yes |
| `fira-code-nf` | Fira Code Nerd Font Mono | `FiraCode Nerd Font Mono` | OFL-1.1 | Yes |
| `hack-nf` | Hack Nerd Font Mono | `Hack Nerd Font Mono` | MIT | Yes |
| `cascadia-code-nf` | Cascadia Code Nerd Font Mono | `CaskaydiaCove Nerd Font Mono` | OFL-1.1 | Yes |
| `iosevka-term-nf` | Iosevka Term Nerd Font Mono | `IosevkaTerm Nerd Font Mono` | OFL-1.1 | Yes |
| `meslo-lgs-nf` | Meslo LG S Nerd Font Mono | `MesloLGS Nerd Font Mono` | Apache-2.0 | Yes |
| `source-code-pro-nf` | Source Code Pro Nerd Font Mono | `SauceCodePro Nerd Font Mono` | OFL-1.1 | Yes |
| `ubuntu-mono-nf` | Ubuntu Mono Nerd Font Mono | `UbuntuMono Nerd Font Mono` | UFL-1.0 | Yes |
| `inconsolata-nf` | Inconsolata Nerd Font Mono | `Inconsolata Nerd Font Mono` | OFL-1.1 | Yes |
| `roboto-mono-nf` | Roboto Mono Nerd Font Mono | `RobotoMono Nerd Font Mono` | Apache-2.0 | Yes |

### Expanded catalog (wave 2)

| ID | Display Name | Family (internal) | License | Nerd Font |
|----|-------------|-------------------|---------|-----------|
| `monaspace-neon-nf` | Monaspace Neon Nerd Font Mono | `MonaspiceNe Nerd Font Mono` | OFL-1.1 | Yes |
| `monaspace-argon-nf` | Monaspace Argon Nerd Font Mono | `MonaspiceAr Nerd Font Mono` | OFL-1.1 | Yes |
| `monaspace-xenon-nf` | Monaspace Xenon Nerd Font Mono | `MonaspiceXe Nerd Font Mono` | OFL-1.1 | Yes |
| `monaspace-radon-nf` | Monaspace Radon Nerd Font Mono | `MonaspiceRn Nerd Font Mono` | OFL-1.1 | Yes |
| `monaspace-krypton-nf` | Monaspace Krypton Nerd Font Mono | `MonaspiceKr Nerd Font Mono` | OFL-1.1 | Yes |
| `commit-mono-nf` | Commit Mono Nerd Font Mono | `CommitMono Nerd Font Mono` | OFL-1.1 | Yes |
| `geist-mono-nf` | Geist Mono Nerd Font Mono | `GeistMono Nerd Font Mono` | OFL-1.1 | Yes |
| `victor-mono-nf` | Victor Mono Nerd Font Mono | `VictorMono Nerd Font Mono` | OFL-1.1 | Yes |
| `ibm-plex-mono-nf` | IBM Plex Mono Nerd Font Mono | `BlexMono Nerd Font Mono` | OFL-1.1 | Yes |
| `hasklig-nf` | Hasklig Nerd Font Mono | `Hasklug Nerd Font Mono` | OFL-1.1 | Yes |
| `0xproto-nf` | 0xProto Nerd Font Mono | `0xProto Nerd Font Mono` | OFL-1.1 | Yes |
| `intel-one-mono-nf` | Intel One Mono Nerd Font Mono | `IntoneMono Nerd Font Mono` | OFL-1.1 | Yes |
| `lilex-nf` | Lilex Nerd Font Mono | `Lilex Nerd Font Mono` | OFL-1.1 | Yes |
| `space-mono-nf` | Space Mono Nerd Font Mono | `SpaceMono Nerd Font Mono` | OFL-1.1 | Yes |
| `dejavu-sans-mono-nf` | DejaVu Sans Mono Nerd Font Mono | `DejaVuSansM Nerd Font Mono` | custom | Yes |
| `cousine-nf` | Cousine Nerd Font Mono | `Cousine Nerd Font Mono` | Apache-2.0 | Yes |
| `bigblue-terminal-437-nf` | BigBlue Terminal 437 Nerd Font Mono | `BigBlueTerm437 Nerd Font Mono` | CC-BY-SA-4.0 | Yes |
| `bigblue-terminal-plus-nf` | BigBlue Terminal Plus Nerd Font Mono | `BigBlueTermPlus Nerd Font Mono` | CC-BY-SA-4.0 | Yes |
| `terminus-nf` | Terminus Nerd Font Mono | `Terminess Nerd Font Mono` | OFL-1.1 | Yes |
| `profont-iix-nf` | ProFont IIx Nerd Font Mono | `ProFont IIx Nerd Font Mono` | MIT | Yes |
| `profont-windows-nf` | ProFont Windows Nerd Font Mono | `ProFontWindows Nerd Font Mono` | MIT | Yes |
| `go-mono-nf` | Go Mono Nerd Font Mono | `GoMono Nerd Font Mono` | BSD-3-Clause | Yes |
| `d2coding-ligature-nf` | D2Coding Ligature Nerd Font Mono | `D2CodingLigature Nerd Font Mono` | OFL-1.1 | Yes |

All Nerd Font variants are sourced from the official
[Nerd Fonts releases](https://github.com/ryanoasis/nerd-fonts/releases/latest).
The plain JetBrains Mono is from the
[JetBrains/JetBrainsMono](https://github.com/JetBrains/JetBrainsMono/releases/latest) release.

Notes:
- **Cascadia Code** → the Nerd Font patch is named `CaskaydiaCove`; the internal family
  name differs from the display name accordingly.
- **Source Code Pro** → the Nerd Font patch is named `SauceCodePro`; same situation.
- **IBM Plex Mono** → the Nerd Font patch is named `BlexMono`; internal family differs.
- **IntelOne Mono** → the Nerd Font patch is named `IntoneMono`; internal family differs.
- **Hasklig** → the Nerd Font patch is named `Hasklug`; uses `.otf` format.
- **Monaspace** is a superfamily of 5 optical-size styles (Neon/Argon/Xenon/Radon/Krypton),
  each a separate manifest entry; all use `.otf` format.
- **CommitMono** and **GeistMono** use `.otf` format.
- **DejaVu Sans Mono** → the Nerd Font patch is named `DejaVuSansM`; uses the Bitstream Vera /
  Arev custom license (see `licenses/DejaVuSansMono-LICENSE.txt`).
- **Terminus** → the Nerd Font patch is named `Terminess`.
- **Iosevka Term** → large files (~13 MB each); the user is warned before download.
- **Meslo LG S** is the Small-Line-Gap variant, the most common choice for Powerline terminals.
- **BigBlueTerminal** ships two separate variants: 437 (IBM PC character set) and Plus (extended).
- **ProFont** ships two variants: IIx (original proportions) and Windows (Windows bitmap-derived).
- **BigBlueTerminal**, **ProFont** → no Bold variant available; Regular only.

---

## Adding a New Font

1. Download the Regular and Bold `.ttf` files into this directory.
2. Verify the internal family name:
   ```bash
   fc-scan --format '%{family}\n' <file.ttf>
   ```
3. Add a row to the `add_meta` table in `gen_manifest.sh`:
   ```bash
   add_meta "FileStemPrefix" \
            "my-font-id" \
            "My Font Display Name" \
            "My Font Internal Family" \
            "OFL-1.1" \
            "true"
   ```
   where `FileStemPrefix` is the common part of `<prefix>-Regular.ttf` and `<prefix>-Bold.ttf`.
4. Add the corresponding license file to `licenses/`.
5. Run the generator:
   ```bash
   cd muxr-core/fonts && ./gen_manifest.sh
   ```
6. Verify:
   ```bash
   python3 -c "import json;d=json.load(open('manifest.json'));print(len(d['fonts']),'families')"
   ```

---

## Regenerating manifest.json

```bash
cd muxr-core/fonts && ./gen_manifest.sh
```

The script is idempotent and deterministic: it scans `*.ttf`/`*.otf` in this directory,
recomputes `bytes` and `sha256` from the actual files, and rewrites `manifest.json`.
Run it any time after adding, removing, or replacing font files.

Each file's `url` is built as `$FONTS_BASE_URL/<filename>`, where `FONTS_BASE_URL`
defaults to `https://muxr.app/fonts` (the canonical hosting location) and can be
overridden — `FONTS_BASE_URL=... ./gen_manifest.sh` — if the hosting location ever
changes.

**Fail-closed on missing binaries:** every family in the metadata table must match at
least one file on disk, or the script errors out naming the family and exits 1 rather
than silently writing a `manifest.json` that's missing that family's files. To
intentionally regenerate a manifest from a partial local checkout, set `SKIP_MISSING=1`
to downgrade the error to a warning and omit the missing families instead.

---

## Bootstrap & disaster recovery

The font catalog has a circular origin: the default download source for every file is
`https://muxr.app/fonts/`, which is served by the very website image this catalog builds.
Two consequences follow directly:

- **Adding a new font:** `fetch_fonts.sh`'s skip-if-valid check accepts pre-seeded files,
  so place the new Regular/Bold binaries in `website/fonts/` locally first, regenerate
  `manifest.json` (above), then build + deploy the image — locally, or via CI with the new
  file already present in the build context. Only once that image is live does the new
  file become fetchable from muxr.app; dispatching a build before the binary is seeded
  locally deadlocks (the site can't serve what it doesn't have yet).
- **Recovering when muxr.app is down:** any machine holding a valid `website/fonts/`
  directory (all files matching the committed manifest) can rebuild and redeploy directly
  — no fetch needed. Without one, pull the *previous* image and extract it:
  `docker create` the image, then `docker cp <container>:/usr/share/nginx/html/fonts
  website/fonts`. For a CI rebuild specifically, the `website-release.yml` workflow's
  `fonts_base_url` dispatch input (or the `FONTS_BASE_URL` env var directly) points
  `fetch_fonts.sh` at a temporary https mirror serving the same filenames instead of the
  unreachable default — the manifest sha256 check still gates what lands in the image.

---

## Licenses

License files for bundled fonts are in `licenses/`. See that directory for the full
texts. Font licenses apply to the `.ttf` files only — not to the Muxr app code.
