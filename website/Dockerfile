FROM nginx:1.27-alpine

COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY index.html docs.html guide.html /usr/share/nginx/html/
COPY assets/ /usr/share/nginx/html/assets/
# Font catalog: run ./fetch_fonts.sh first — fonts/ is gitignored build context.
COPY fonts/ /usr/share/nginx/html/fonts/

EXPOSE 80
