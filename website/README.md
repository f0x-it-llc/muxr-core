# Muxr Website

A static HTML website for Muxr, the Flutter mobile and tablet client for controlling remote herdr and zellij terminal-multiplexer sessions. The site provides an interactive introduction, getting-started documentation, usage guides, and the legal/support pages required for app-store submission — plus the Nerd Font catalog the app downloads from.

## Local Preview (No Docker)

To preview the website locally without Docker:

```bash
cd website
python3 -m http.server 8000
```

Then open `http://localhost:8000` in your browser. Python's simple HTTP server avoids path and clipboard quirks that can occur when opening via `file://`.

## Docker Build & Run

To build and run the site as a Docker container:

```bash
cd website && ./fetch_fonts.sh   # populate fonts/ (gitignored) before building
docker build -t muxr-web .
docker run --rm -p 8080:80 muxr-web
```

Then open `http://localhost:8080` in your browser.

The build context is `website/`; the image is based on `nginx:1.27-alpine` and serves the site from `/usr/share/nginx/html/`.

## Site Structure

Design: **Glyph** — a single stylesheet (`assets/css/glyph.css`) shared by every page, set in Space Mono (headings) and IBM Plex Mono (body/code), both loaded from Google Fonts.

- **`index.html`** — Landing page with features, how-it-works, and CTAs.
- **`docs.html`** — Getting Started guide: prerequisites, installation, token creation, and pairing.
- **`guide.html`** — Usage Guide: gestures, command center, keyboard bar, fullscreen modes, tablet differences, and settings.
- **`privacy.html`** — Privacy Policy: what the app, site, and optional Muxr Push relay do (and don't) collect. Serves as the Privacy Policy URL required by the Apple App Store and Google Play listings.
- **`terms.html`** — Terms of Use / End User License Agreement (custom EULA) covering licence, subscription/purchase terms, and disclaimers. Required by the App Store and Play Store as the app's terms/EULA link.
- **`support.html`** — Support page: contact info, bug reporting, and an FAQ. Serves as the Support URL required by both app stores.
- **`assets/`** — Static resources:
  - `css/glyph.css` — The Glyph design system: tokens, nav, footer, doc layout, and all shared components in one file.
  - `js/main.js` — Mobile nav toggle, copy-code buttons, scroll-spy active sections.
  - `img/logo.svg` — 4-square grid mark (also favicon).

All six pages share the same `.site-nav` header and `.site-footer` footer (Home, Docs, Guide, Support, Privacy, Terms, GitHub).

## Font Catalog (`/fonts/`)

The app downloads Nerd Fonts at runtime from `muxr.app/fonts/` rather than bundling them. The font binaries are **not** committed to git — `fonts/` is gitignored — because they're large; `muxr.app/fonts/` (this site's own nginx `/fonts/` location) is the canonical, immutable hosting location, pinned by the checked-in `../fonts/manifest.json`. There is no GitHub release involved — the fonts used to be distributed as `muxr-core` `fonts-v1` release assets, but GitHub's anonymous release-download edge blocks non-browser clients, so hosting moved to muxr.app and the release is gone.

Before building the Docker image, populate `fonts/` from muxr.app:

```bash
cd website && ./fetch_fonts.sh
```

The script (`curl` + `python3`, no `gh` CLI) downloads each file listed in the local `../fonts/manifest.json` from `$FONTS_BASE_URL` (default `https://muxr.app/fonts`), skipping any file already present with a matching sha256, and hard-verifies every file's sha256 against the manifest — a mismatch or missing file fails the script before anything reaches an image build.

## nginx & CSP

`nginx.conf` serves the site with security headers on every response (`X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`, `Strict-Transport-Security`, and a `Content-Security-Policy`). The CSP is `default-src 'self'` plus explicit allowances for the Google Fonts stylesheet/font origins — no inline `<style>`/`<script>` and no other third-party origins are permitted. `/assets/` and `/fonts/` are cached as long-lived immutable; `*.html` responses are `no-cache` so deploys are picked up immediately.

## Links

- **GitHub:** [`f0x-it-llc/muxr-core`](https://github.com/f0x-it-llc/muxr-core) (open-source MIT backend)
- **Server:** Rust gRPC backend and TUI pairing tool (`muxrctl`)
- **App:** Closed-source Flutter mobile/tablet client

The backend is self-hosted and open-source; the Flutter app is closed-source.
