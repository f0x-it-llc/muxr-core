FROM nginx:1.27-alpine

# Links the GHCR package to this repo (public, shows on the repo's Packages
# sidebar) — GHCR reads this label from pushed images.
LABEL org.opencontainers.image.source="https://github.com/f0x-it-llc/muxr-core" \
      org.opencontainers.image.description="muxr.app website + font catalog" \
      org.opencontainers.image.licenses="PolyForm-Shield-1.0.0"

COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY index.html docs.html advanced.html guide.html privacy.html terms.html support.html /usr/share/nginx/html/
COPY robots.txt sitemap.xml llms.txt favicon.png /usr/share/nginx/html/
COPY assets/ /usr/share/nginx/html/assets/
# Font catalog: run ./fetch_fonts.sh first — fonts/ is gitignored build context.
COPY fonts/ /usr/share/nginx/html/fonts/

EXPOSE 80
