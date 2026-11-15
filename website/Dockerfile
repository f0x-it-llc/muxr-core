# Stage 1: generate the markdown mirrors (<page>.html.md) and llms-full.txt
# from the HTML pages. They are build output and are never committed.
FROM python:3.13-alpine AS md

WORKDIR /src
COPY *.html sitemap.xml ./
COPY how-to/ how-to/
COPY compare/ compare/
COPY alternatives/ alternatives/
COPY best/ best/
COPY tools/ tools/
RUN python3 tools/build_md.py --src /src --out /out

# Stage 2: the served image.
FROM nginx:1.27-alpine

# Links the GHCR package to this repo (public, shows on the repo's Packages
# sidebar) — GHCR reads this label from pushed images.
LABEL org.opencontainers.image.source="https://github.com/f0x-it-llc/muxr-core" \
      org.opencontainers.image.description="muxr.app website + font catalog" \
      org.opencontainers.image.licenses="PolyForm-Shield-1.0.0"

COPY nginx.conf /etc/nginx/conf.d/default.conf
# Explicit globs only, never `COPY . .`: CLAIMS.md, PAGE_RULES.md, README.md,
# tools/ and the build files must never reach the web root. Root-level *.txt
# covers robots.txt, llms.txt and any key file added beside them.
COPY *.html *.txt *.xml favicon.png /usr/share/nginx/html/
COPY how-to/ /usr/share/nginx/html/how-to/
COPY compare/ /usr/share/nginx/html/compare/
COPY alternatives/ /usr/share/nginx/html/alternatives/
COPY best/ /usr/share/nginx/html/best/
COPY assets/ /usr/share/nginx/html/assets/
# Font catalog: run ./fetch_fonts.sh first — fonts/ is gitignored build context.
COPY fonts/ /usr/share/nginx/html/fonts/
# Generated mirrors land beside their pages; llms-full.txt in the web root.
COPY --from=md /out/ /usr/share/nginx/html/

EXPOSE 80
