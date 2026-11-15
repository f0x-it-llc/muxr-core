# Muxr Website

A static HTML website for Muxr, the [Frust](https://frust.dev)-native mobile and tablet client for controlling remote herdr and zellij terminal-multiplexer sessions. The site provides an interactive introduction, getting-started documentation, usage guides, and the legal/support pages required for app-store submission — plus the Nerd Font catalog the app downloads from.

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

The build has two stages. A Python stage (`python:3.13-alpine`) runs `tools/build_md.py` over the HTML pages and generates the markdown mirrors and `llms-full.txt`; the nginx stage copies the site files and then the generated output on top. The Dockerfile copies explicit globs and folders, never `COPY . .`, so `CLAIMS.md`, `PAGE_RULES.md`, `README.md` and `tools/` are internal and never shipped. **A new section folder (like `how-to/` or `best/`) needs a `COPY` line in both stages** of `Dockerfile`, or its pages are missing from the mirrors or from the image.

## Site Structure

Design: **Glyph** — a single stylesheet (`assets/css/glyph.css`) shared by every page, set in Space Mono (headings) and IBM Plex Mono (body/code), both loaded from Google Fonts.

- **`index.html`** — Landing page with features, how-it-works, a coding-agent section, a short comparison, an FAQ teaser and CTAs.
- **`docs.html`** — Quick start in five steps: get the app, install the server, reach it over a tailnet, run the `muxrctl` wizard, scan the pairing QR.
- **`advanced.html`** — Advanced setup: the `muxrctl` dashboard keys, the `muxrd` command-line route, LAN, a public domain, and the Docker dev rig. Linked from `docs.html`, not from the top nav.
- **`guide.html`** — Usage Guide: gestures, command center, keyboard bar, fullscreen modes, tablet differences, and settings.
- **`privacy.html`** — Privacy Policy: what the app, site, and optional Muxr Push relay do (and don't) collect. Serves as the Privacy Policy URL required by the Apple App Store and Google Play listings.
- **`terms.html`** — Terms of Use / End User License Agreement (custom EULA) covering licence, purchase terms, and disclaimers. Required by the App Store and Play Store as the app's terms/EULA link.
- **`support.html`** — Support page: contact info, bug reporting, and an FAQ. Serves as the Support URL required by both app stores.
- **`about.html`, `faq.html`, `pricing.html`, `security.html`, `roadmap.html`** — Root-level answer pages: what Muxr is, the FAQ, pricing, security, and the roadmap.
- **`how-to/`** — `index.html` plus four guides: zellij on iPhone and iPad, zellij on Android, herdr on your phone, Claude Code and Codex on your phone.
- **`compare/`** — `index.html` plus eight comparisons: Moshi, Zelmux, Termius, Blink Shell, the zellij web client, an SSH app running zellij, the Herdr mobile app, Claude Code Remote Control.
- **`alternatives/`** — Moshi and Termius alternatives.
- **`best/`** — Best-of lists: zellij clients, herdr clients, apps for coding agents.
- **`robots.txt`**, **`sitemap.xml`**, **`llms.txt`**, **`indexnow-key.txt`** — Crawler, AI-agent and IndexNow files served from the web root (`favicon.png` sits beside them). `llms.txt` links the markdown mirror of every page; `llms-full.txt` (generated, see below) holds the whole site. Keep each `<lastmod>` in `sitemap.xml` in step with page changes.
- **`tools/`** — `build_md.py` (markdown mirrors), `check_site.py` (site checks) and `indexnow.sh` (IndexNow ping).
- **`CLAIMS.md`**, **`PAGE_RULES.md`** — Internal; see "Facts and claims". Never shipped.
- **`assets/`** — Static resources:
  - `css/glyph.css` — The Glyph design system: tokens, nav, footer, doc layout, and all shared components in one file. `/assets/` is cached as immutable for 30 days, so every page links it as `glyph.css?v=N` (currently `?v=4`): raise `N` on all pages whenever the stylesheet changes, and give a changed image a new file name.
  - `js/main.js` — Mobile nav toggle, copy-code buttons, scroll-spy active sections, and the phone-width "On this page" toggle/breadcrumb injected into `.doc-sidebar` (paired with glyph.css's `html.js` gating and the `--nav-h` token).
  - `img/logo.svg` — 4-square grid mark (also favicon).
  - `img/screenshots/` — real app screenshots (WebP, 640 px wide) generated from the store-listing captures, plus the `muxrctl` captures (WebP, cropped to the dialog, about 1200 px wide): `wizard-1-welcome.webp`, `wizard-2-network.webp`, `wizard-3-certificate.webp`, `wizard-4-token.webp`, `wizard-5-daemon.webp`, `wizard-6-pair.webp` (on `docs.html`) and `dashboard.webp` (on `advanced.html`); replace by regenerating, never by editing.

Every page shares the same `.site-nav` header and `.site-footer` footer; new pages copy them from `about.html` and use root-absolute paths, because they live in subdirectories.

## Font Catalog (`/fonts/`)

The app downloads Nerd Fonts at runtime from `muxr.app/fonts/` rather than bundling them. The font binaries are **not** committed to git — `fonts/` is gitignored — because they're large; `muxr.app/fonts/` (this site's own nginx `/fonts/` location) is the canonical, immutable hosting location, pinned by the checked-in `../fonts/manifest.json`. The fonts used to be distributed as `muxr-core` `fonts-v1` release assets, but GitHub's anonymous release-download edge blocks non-browser clients, so hosting moved to muxr.app; the `fonts-v1` release has since been deleted. The catalog itself is served here too — `fetch_fonts.sh` copies `../fonts/manifest.json` into the build context, and nginx serves it at `/fonts/manifest.json` with `no-cache` (unlike the immutable binaries) so the app's discovery root updates on every deploy and has no GitHub dependency.

Before building the Docker image, populate `fonts/` from muxr.app:

```bash
cd website && ./fetch_fonts.sh
```

The script (`curl` + `python3`, no `gh` CLI) downloads each file listed in the local `../fonts/manifest.json` from that entry's own `url` (pointing at muxr.app), or from `$FONTS_BASE_URL/<filename>` if that env var is set (disaster recovery / migration override — https-only, enforced per file). It skips any file already present with a matching sha256, downloads to a temp file and only renames it into place once the sha256 verifies, prunes any stray binary not listed in the manifest, and hard-verifies every file before exiting 0 — a mismatch, a non-https source, or a missing file fails the script before anything reaches an image build. See `../fonts/README.md`'s "Bootstrap & disaster recovery" section for the circular-origin caveat (the default font source is this site itself) and how to recover if muxr.app is down.

## nginx & CSP

`nginx.conf` serves the site with security headers on every response (`X-Content-Type-Options`, `X-Frame-Options`, `Referrer-Policy`, `Strict-Transport-Security`, and a `Content-Security-Policy`). The CSP is `default-src 'self'` plus explicit allowances for the Google Fonts stylesheet/font origins — no inline `<style>`/`<script>` and no other third-party origins are permitted. `/assets/` and `/fonts/` are cached as long-lived immutable; `*.html` responses are `no-cache` so deploys are picked up immediately.

## Facts and claims

`CLAIMS.md` is the source of every fact on the site: what Muxr is, and what other products offer, each with its source URL and fetch date. `PAGE_RULES.md` is the contract every new page follows (head, JSON-LD, paths, body, writing, URL map). Both are internal and never shipped. To update a statement about another product, re-check the source, change its entry and fetch date in `CLAIMS.md`, then change every page that quotes it and the visible "as published on" date. A fact missing from `CLAIMS.md` is written "Not stated" or left out, never guessed.

## Price wording

Wherever a price is given, the page uses this sentence verbatim:

> Muxr Pro is a one-time $19.99 purchase, 50% off ($9.99) until January 13, 2027.

It appears in `llms.txt`, on the homepage, `about.html`, `faq.html`, `pricing.html`, and in the comparison, how-to, alternatives and best-of pages. After 13 January 2027 the sale clause is removed everywhere with one search: find `, 50% off ($9.99) until January 13, 2027` and delete it, and update `PRICE_SENTENCE` in `tools/check_site.py` to match.

The site must not be deployed before the muxr-app licence change merges (see the release note in `CLAIMS.md`).

## Markdown mirrors

Every page has a markdown mirror at its URL plus `.md` (for example `https://muxr.app/faq.html.md`; the homepage is `/index.html.md` and section indexes are `/how-to/index.html.md` and `/compare/index.html.md`), advertised by a `<link rel="alternate" type="text/markdown">` in each page. They and `llms-full.txt` are generated in the Docker `md` stage, never committed (they are gitignored). nginx serves `.html.md` files as `text/markdown` and `llms-full.txt` beside `llms.txt`. To generate them locally:

```bash
cd website
python3 tools/build_md.py --src . --out /tmp/muxr-md
```

## Checks

`python3 tools/check_site.py`, run from `website/`, enumerates the pages on disk (skipping dot-directories and `assets/`, `fonts/`, `tools/`, `node_modules/`, the same rules `tools/build_md.py` uses) and fails (exit 1, one line per problem) on: title, description or canonical problems, a missing markdown alternate link, unparseable or wrong-type JSON-LD, a broken internal link or source, a relative link in a subdirectory page, a page missing from `sitemap.xml` or from `llms.txt`, "open source" said about Muxr (in body text, including text split by inline tags such as links, in meta content and in any JSON-LD string), Muxr's price stated other than in the verbatim sentence, a word count outside the `PAGE_RULES.md` range, and inline styles or scripts. It also fails on:

- **External subresources**: a `<script>`, `<img>`, `<iframe>`, `<source>`, `<video>`, `<audio>`, `<object>`, `<embed>` or a fetching `<link>` (stylesheet, preload, preconnect, icon, manifest) loaded from any host other than the site itself and the two font hosts the CSP in `nginx.conf` allows (`fonts.googleapis.com` for the stylesheet, `fonts.gstatic.com` for the font files). Ordinary `<a href>` links to other sites are fine.
- **Publisher disclosure**: a page under `compare/`, `alternatives/` or `best/` whose visible text does not contain the phrase "makers of Muxr".

It uses only the standard library. The tools have unit tests (`tools/test_build_md.py`, `tools/test_check_site.py`) that build their fixtures in a temporary directory; run them from `website/`:

```bash
cd website
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tools -p 'test_*.py' -v
```

CI runs the unit tests, `tools/check_site.py` and `tools/build_md.py` in the `website` job of `.github/workflows/ci.yml` on every push and pull request.

## Indexing

`indexnow-key.txt` holds the site's IndexNow key (public by design) and is served at `https://muxr.app/indexnow-key.txt`. After a deploy, run `tools/indexnow.sh --send` to POST the `sitemap.xml` URL list to IndexNow; it prints the HTTP status and exits non-zero unless IndexNow answers 200 or 202. Without `--send` it only prints the payload.

## Links

- **GitHub:** [`f0x-it-llc/muxr-core`](https://github.com/f0x-it-llc/muxr-core) (source-available backend, PolyForm Shield 1.0.0)
- **Server:** Rust gRPC backend and TUI pairing tool (`muxrctl`)
- **App:** [Frust](https://frust.dev)-native mobile/tablet client; its source is published under the PolyForm Shield License 1.0.0 at https://github.com/f0x-it-llc/muxr-app

The backend is self-hosted and source-available.
