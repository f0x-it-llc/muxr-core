# Muxr Website

A static HTML website for Muxr, the Flutter mobile and tablet client for controlling remote herdr and zellij terminal-multiplexer sessions. The site provides an interactive introduction, getting-started documentation, and usage guides.

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
docker build -t muxr-web website/
docker run --rm -p 8080:80 muxr-web
```

Then open `http://localhost:8080` in your browser.

The build context is `website/`; the image is based on `nginx:1.27-alpine` and serves the site from `/usr/share/nginx/html/`.

## Site Structure

- **`index.html`** — Landing page with features, how-it-works, and CTAs.
- **`docs.html`** — Getting Started guide: prerequisites, installation, token creation, and pairing.
- **`guide.html`** — Usage Guide: gestures, command center, keyboard bar, fullscreen modes, tablet differences, and settings.
- **`assets/`** — Static resources:
  - `css/tokens.css` — Design tokens (colors, typography, spacing).
  - `css/base.css` — Global styles, nav, footer, and shared components.
  - `css/docs.css` — Doc/guide layout (sidebar + content).
  - `js/main.js` — Mobile nav toggle, copy-code buttons, scroll-spy active sections.
  - `img/logo.svg` — 4-square grid mark (also favicon).

## Links

- **GitHub:** [`f0x-it-llc/muxr-core`](https://github.com/f0x-it-llc/muxr-core) (open-source MIT backend)
- **Server:** Rust gRPC backend and TUI pairing tool (`muxrctl`)
- **App:** Closed-source Flutter mobile/tablet client

The backend is self-hosted and open-source; the Flutter app is closed-source.
