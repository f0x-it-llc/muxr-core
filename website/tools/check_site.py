#!/usr/bin/env python3
"""Check the muxr.app static site against website/PAGE_RULES.md.

Run from anywhere:

    python3 tools/check_site.py

Prints every failure and exits 1, or prints a summary and exits 0. Standard
library only. Pages are enumerated from disk (every *.html below the site
root, minus dot-directories and assets/fonts/tools/node_modules, the same
rules tools/build_md.py uses), never from a hard-coded list, apart from the
seven original pages that keep their own layout and are exempt from the
new-page rules.

run(root) does the work and returns the failures as a list of strings, so the
checks can be called from tools/test_check_site.py on a fixture tree.
"""

import json
import os
import re
import sys
import xml.etree.ElementTree as ET
from html import unescape
from html.parser import HTMLParser
from urllib.parse import urlsplit

BASE = "https://muxr.app"
SELF_HOST = "muxr.app"

# Directories that never hold pages, at any depth. Directories whose name
# starts with "." are skipped as well.
SKIP_DIRS = {"assets", "fonts", "tools", "node_modules"}

# The seven original pages: own layout, relative paths allowed, exempt from the
# new-page path, word-count, title-suffix and JSON-LD-type rules.
ORIGINAL_PAGES = {
    "index.html", "docs.html", "advanced.html", "guide.html",
    "support.html", "privacy.html", "terms.html",
}

# Word limits for <main> of new pages (PAGE_RULES.md section 5).
DEFAULT_WORDS = (400, 900)
WORD_EXCEPTIONS = {"about.html": (300, 500), "faq.html": (400, 1800)}

PRICE_SENTENCE = ("Muxr Pro is a one-time $19.99 purchase, "
                  "50% off ($9.99) until January 13, 2027.")
DISAMBIGUATION = ("Muxr by F0X IT LLC (muxr.app) is a zellij and herdr client "
                  "and is unrelated to the npm package @trymuxr/cli at "
                  "trymuxr.com.")

# Pages in these sections compare Muxr with other products and must say, in
# their visible text, who published them.
DISCLOSURE_SECTIONS = ("compare/", "alternatives/", "best/")
DISCLOSURE = "makers of Muxr"

# Names of other products, as the pages write them (CLAIMS.md "Other
# products"). A "9.99" figure outside Muxr's own price sentence is accepted
# only when one of these names appears near it (another product's price).
# Product names only: a generic phrase is no evidence that the figure belongs
# to another product.
OTHER_PRODUCTS = re.compile(r"\b(?:" + "|".join(re.escape(name) for name in [
    "Moshi", "Zelmux", "Termius", "Blink", "Prompt 3", "ShellFish", "Termux",
    "ConnectBot", "zellij web client", "Herdr mobile", "herdr-mobile-relay",
    "Heeler", "Collie", "Sherpr", "herdrup", "Whip",
    "Claude Code Remote Control", "Codex", "Happy", "Tactic Remote",
    "Cosyra", "KittyLitter",
]) + r")\b")

# Exact exceptions to the "open source" rule: whole sentences that call Blink
# Shell open source (CLAIMS.md records it as such, with its source link) and
# name Muxr only by contrast. Keyed by page; a sentence is exempt only when it
# equals one of these strings after whitespace normalisation.
OPEN_SOURCE_EXCEPTIONS = {
    "compare/blink-shell.html": {
        "Blink Shell is a general SSH and Mosh terminal for iPhone and iPad, "
        "and it is open source under GPL-3.0; Muxr is a zellij and herdr "
        "client for iOS and Android that needs a daemon on the server.",
    },
    "compare/index.html": {
        "An open-source SSH and Mosh terminal for iOS against Muxr.",
    },
}

# The only external hosts a page may load anything from, and what for. They
# match the Content-Security-Policy in nginx.conf: the font stylesheet and the
# font files. Every other subresource comes from the site itself; ordinary
# <a href> links to other sites are not subresources and are not limited.
EXTERNAL_SUBRESOURCES = {
    "fonts.googleapis.com": {"stylesheet", "preconnect"},
    "fonts.gstatic.com": {"preconnect", "preload"},
}

# <link rel> values that make the browser fetch or contact the href.
LINK_FETCH_RELS = {
    "stylesheet", "preload", "modulepreload", "preconnect", "dns-prefetch",
    "prefetch", "prerender", "icon", "apple-touch-icon", "mask-icon",
    "manifest",
}

# Attributes that load a subresource, per element.
SUBRESOURCE_ATTRS = {
    "script": ("src",),
    "img": ("src", "srcset"),
    "iframe": ("src",),
    "frame": ("src",),
    "source": ("src", "srcset"),
    "video": ("src", "poster"),
    "audio": ("src",),
    "track": ("src",),
    "object": ("data",),
    "embed": ("src",),
    "input": ("src",),
}

# Elements that do not end a sentence: their text is read as part of the
# surrounding text. Every other tag starts a new line.
INLINE_TAGS = {
    "a", "span", "strong", "em", "code", "b", "i", "abbr", "small", "kbd",
    "samp", "sub", "sup", "mark", "s", "u", "q", "cite", "time", "var",
    "del", "ins", "dfn", "bdi", "bdo", "data", "wbr",
}


class PageParser(HTMLParser):
    """Collects what the checks need from one HTML page."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.titles = []
        self.in_title = False
        self.meta = {}          # name/property -> content
        self.meta_contents = []  # every meta content value, in order
        self.links = []         # (rel, type, href)
        self.refs = []          # (tag, attr, value) for href/src
        self.subresources = []  # (tag, kind, url) the browser would fetch
        self.ids = set()
        self.ldjson = []
        self.in_ld = False
        self.ld_buf = []
        self.bad_scripts = 0
        self.inline_style_tags = 0
        self.style_attrs = 0
        self.h1 = 0
        self.main_depth = 0
        self.main_text = []
        self.skip_depth = 0     # script/style inside main
        self.has_main = False

    def handle_starttag(self, tag, attrs):
        a = {k: (v if v is not None else "") for k, v in attrs}
        if "id" in a:
            self.ids.add(a["id"])
        if "style" in a:
            self.style_attrs += 1
        if tag == "title":
            self.in_title = True
            self.titles.append("")
        elif tag == "meta":
            key = a.get("name") or a.get("property")
            if key:
                self.meta[key] = a.get("content", "")
            if "content" in a:
                self.meta_contents.append(a["content"])
        elif tag == "link":
            self.links.append((a.get("rel", ""), a.get("type", ""),
                               a.get("href", "")))
            for kind in sorted(set(a.get("rel", "").lower().split())
                               & LINK_FETCH_RELS):
                if a.get("href"):
                    self.subresources.append((tag, kind, a["href"]))
        elif tag == "style":
            self.inline_style_tags += 1
        elif tag == "script":
            if a.get("type") == "application/ld+json":
                self.in_ld = True
                self.ld_buf = []
            elif "src" not in a:
                self.bad_scripts += 1
            if self.main_depth:
                self.skip_depth += 1
        elif tag == "h1":
            self.h1 += 1
        for attr in SUBRESOURCE_ATTRS.get(tag, ()):
            value = a.get(attr, "").strip()
            if not value:
                continue
            if attr == "srcset":
                urls = [c.strip().split()[0] for c in value.split(",")
                        if c.strip()]
            else:
                urls = [value]
            for url in urls:
                self.subresources.append((tag, tag, url))
        if tag == "main":
            self.has_main = True
            self.main_depth += 1
        for attr in ("href", "src"):
            if attr in a:
                self.refs.append((tag, attr, a[attr]))

    def handle_startendtag(self, tag, attrs):
        self.handle_starttag(tag, attrs)

    def handle_endtag(self, tag):
        if tag == "title":
            self.in_title = False
        elif tag == "script":
            if self.in_ld:
                self.ldjson.append("".join(self.ld_buf))
                self.in_ld = False
            if self.main_depth and self.skip_depth:
                self.skip_depth -= 1
        elif tag == "main" and self.main_depth:
            self.main_depth -= 1

    def handle_data(self, data):
        if self.in_title:
            self.titles[-1] += data
        if self.in_ld:
            self.ld_buf.append(data)
        if self.main_depth and not self.skip_depth:
            self.main_text.append(data)


def parse_page(source):
    p = PageParser()
    p.feed(source)
    p.close()
    return p


def normalise(text):
    return re.sub(r"\s+", " ", text).strip()


def tag_stripped(raw):
    """Plain text of a page (JSON-LD text included), whitespace-normalised."""
    text = re.sub(r"<[^>]+>", " ", raw)
    return normalise(unescape(text))


def collect_types(node, types):
    """Add every @type found anywhere in a parsed JSON-LD document."""
    if isinstance(node, dict):
        t = node.get("@type")
        if isinstance(t, list):
            types.update(t)
        elif t:
            types.add(t)
        for v in node.values():
            collect_types(v, types)
    elif isinstance(node, list):
        for v in node:
            collect_types(v, types)


def collect_strings(node, out):
    """Append every string value found anywhere in a parsed JSON document."""
    if isinstance(node, str):
        out.append(node)
    elif isinstance(node, dict):
        for v in node.values():
            collect_strings(v, out)
    elif isinstance(node, list):
        for v in node:
            collect_strings(v, out)
    return out


def body_lines(raw):
    """Lines a reader sees in <body>: one per block element, so table cells
    and headings do not run together, while inline elements (links, spans,
    emphasis, code) stay inside the sentence around them."""
    body = re.search(r"<body.*?</body>", raw, re.S | re.I)
    body = body.group(0) if body else raw
    body = re.sub(r"<(script|style)\b.*?</\1\s*>", " ", body, flags=re.S | re.I)
    body = re.sub(r"<!--.*?-->", " ", body, flags=re.S)
    # A hub card is a link holding a title and a description: two lines.
    body = re.sub(r"<span class=\"hub-card[^>]*>", "\n", body)

    # An inline tag adds no space of its own ("<b>open</b>-source" reads
    # "open-source"); any other tag ends the line.
    def replace(m):
        return "" if m.group(1).lower() in INLINE_TAGS else "\n"

    body = re.sub(r"</?([A-Za-z][A-Za-z0-9-]*)\b[^>]*>", replace, body)
    lines = [normalise(unescape(ln)) for ln in body.split("\n")]
    return [ln for ln in lines if ln]


def find_pages(root):
    """Every *.html page under root, as sorted '/'-separated paths. Skips
    dot-directories and SKIP_DIRS at any depth, like tools/build_md.py."""
    pages = []
    for dirpath, dirs, files in os.walk(root):
        rel_dir = os.path.relpath(dirpath, root)
        dirs[:] = sorted(d for d in dirs
                         if not d.startswith(".") and d not in SKIP_DIRS)
        for f in sorted(files):
            if f.endswith(".html"):
                rel = f if rel_dir == "." else os.path.join(rel_dir, f)
                pages.append(rel.replace(os.sep, "/"))
    return sorted(pages)


def page_url(rel):
    """Canonical URL of a page path."""
    if rel == "index.html":
        return BASE + "/"
    if rel.endswith("/index.html"):
        return BASE + "/" + rel[: -len("index.html")]
    return BASE + "/" + rel


def url_to_page(url):
    """Map a site URL back to a page path, or None when it is not a page."""
    if not url.startswith(BASE + "/"):
        return None
    path = url[len(BASE):].split("#")[0].split("?")[0]
    if path.endswith("/"):
        path += "index.html"
    return path.lstrip("/")


def is_section_index(rel):
    return rel.endswith("/index.html")


def external_host(url):
    """Host of a URL that is fetched from another site, else None (relative,
    root-absolute, the site itself, or a non-network scheme such as data:)."""
    u = url.strip()
    if u.startswith("//"):
        u = "https:" + u
    parts = urlsplit(u)
    if parts.scheme.lower() not in ("http", "https"):
        return None
    host = (parts.hostname or "").lower()
    if not host or host == SELF_HOST:
        return None
    return host


def check_subresources(rel, subresources):
    """Fail every subresource loaded from a host the site does not allow."""
    out = []
    for tag, kind, url in subresources:
        host = external_host(url)
        if host is None or kind in EXTERNAL_SUBRESOURCES.get(host, ()):
            continue
        what = f"<link rel={kind}>" if tag == "link" else f"<{tag}>"
        out.append(f"{rel}: {what} loads {url!r} from external host {host} "
                   "(only the site itself and the font hosts in nginx.conf "
                   "are allowed)")
    return out


def check_sentence_rules(rel, lines):
    """No "open source" in the same sentence as Muxr or muxrd."""
    out = []
    sentences = []
    for ln in lines:
        sentences.extend(re.split(r"(?<=[.!?])\s+", normalise(ln)))
    exempt = OPEN_SOURCE_EXCEPTIONS.get(rel, set())
    for sentence in sentences:
        if sentence in exempt:
            continue
        if (re.search(r"open[\s\-]source", sentence, re.I)
                and re.search(r"\bMuxr\b|\bmuxrd\b", sentence)):
            out.append(f'{rel}: "open source" in a sentence naming Muxr/muxrd: '
                       f'{sentence[:160]!r}')
    return out


def check_prices(rel, text):
    """Test Muxr's own price statements, not every "9.99" on the page.

    Muxr's price may only appear as the verbatim sentence
    "Muxr Pro is a one-time $19.99 purchase, 50% off ($9.99) until January
    13, 2027." Pages legitimately also carry other products' prices (Moshi
    "$9.99", Blink "$19.99/year"), so the rule works on what is left after
    removing every verbatim sentence:
      * "Muxr Pro" must not be followed by a dollar amount or sale claim,
      * "50% off" and "January 13, 2027" must not appear,
      * a "9.99" must have another product's name (OTHER_PRODUCTS) within
        300 characters.
    When the sale ends, remove the clause from the sentence everywhere and
    update PRICE_SENTENCE here.
    """
    out = []
    rest = text.replace(PRICE_SENTENCE, " ")
    for m in re.finditer(r"Muxr Pro", rest):
        window = rest[m.end(): m.end() + 100]
        if re.search(r"\$\d|50% off|January 13", window):
            out.append(f"{rel}: Muxr Pro price stated outside the verbatim "
                       f"sentence: {rest[m.start(): m.end() + 100]!r}")
    for needle in ("50% off", "January 13, 2027"):
        if needle in rest:
            out.append(f'{rel}: "{needle}" outside the verbatim price sentence')
    for m in re.finditer(r"9\.99", rest):
        window = rest[max(0, m.start() - 300): m.end() + 300]
        if not OTHER_PRODUCTS.search(window):
            out.append(f"{rel}: 9.99 outside the verbatim price sentence and "
                       "with no other product named nearby: "
                       f"{rest[max(0, m.start() - 60): m.end() + 40]!r}")
    if rel == "pricing.html" and PRICE_SENTENCE not in text:
        out.append(f"{rel}: pricing.html lacks the verbatim price sentence")
    return out


def read_text(path):
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def run(root):
    """Check the site rooted at `root` and return every failure."""
    errors = []

    def fail(page, msg):
        errors.append(f"{page}: {msg}")

    def exists(rel_path):
        return os.path.isfile(os.path.join(root, *rel_path.split("/")))

    pages = find_pages(root)
    page_set = set(pages)
    parsed = {}
    sources = {}
    for rel in pages:
        try:
            sources[rel] = read_text(os.path.join(root, *rel.split("/")))
        except (OSError, UnicodeDecodeError) as exc:
            fail(rel, f"cannot read: {exc}")
            continue
        parsed[rel] = parse_page(sources[rel])

    titles_seen = {}

    for rel in pages:
        if rel not in parsed:
            continue
        p = parsed[rel]
        raw = sources[rel]
        original = rel in ORIGINAL_PAGES
        index = is_section_index(rel)
        url = page_url(rel)
        depth = rel.count("/")

        # Title.
        if len(p.titles) != 1:
            fail(rel, f"expected exactly one <title>, found {len(p.titles)}")
        else:
            title = p.titles[0].strip()
            if len(title) > 60:
                fail(rel, f"title is {len(title)} characters (max 60): {title!r}")
            if title in titles_seen:
                fail(rel, f"title duplicates {titles_seen[title]}: {title!r}")
            titles_seen[title] = rel
            if not original and not title.endswith(" · Muxr"):
                fail(rel, f"title must end ' · Muxr': {title!r}")

        # Description.
        desc = p.meta.get("description")
        if not desc:
            fail(rel, "missing meta description")
        elif len(desc) > 155:
            fail(rel, f"meta description is {len(desc)} characters (max 155)")

        # Canonical and markdown alternate.
        canon = [h for r, t, h in p.links if r == "canonical"]
        if len(canon) != 1:
            fail(rel, f"expected one canonical link, found {len(canon)}")
        elif canon[0] != url:
            fail(rel, f"canonical {canon[0]!r} does not match path (expected {url!r})")
        alts = [h for r, t, h in p.links
                if r == "alternate" and t == "text/markdown"]
        if len(alts) != 1:
            fail(rel, f"expected one rel=alternate text/markdown link, found {len(alts)}")
        elif alts[0] != "/" + rel + ".md":
            fail(rel, f"markdown alternate {alts[0]!r} should be {'/' + rel + '.md'!r}")

        # JSON-LD.
        # The original pages other than index.html carry no JSON-LD by design.
        if not p.ldjson and (not original or rel == "index.html"):
            fail(rel, "no JSON-LD block")
        parsed_ld = []
        for block in p.ldjson:
            try:
                parsed_ld.append(json.loads(block))
            except ValueError as exc:
                fail(rel, f"JSON-LD does not parse: {exc}")
        if not original:
            if len(p.ldjson) != 1:
                fail(rel, f"expected exactly one JSON-LD block, found {len(p.ldjson)}")
            types = set()
            for doc in parsed_ld:
                collect_types(doc, types)
            if index:
                need = {"CollectionPage", "ItemList", "BreadcrumbList"}
            elif rel.startswith("how-to/"):
                need = {"TechArticle", "BreadcrumbList"}
            else:
                need = {"Article", "BreadcrumbList"}
            if not need <= types:
                fail(rel, f"JSON-LD types {sorted(t for t in types if t)} lack {sorted(need - types)}")
            if p.meta.get("og:url") != url:
                fail(rel, f"og:url {p.meta.get('og:url')!r} should equal the canonical {url!r}")
            if p.h1 != 1:
                fail(rel, f"expected exactly one <h1>, found {p.h1}")
            if not p.has_main:
                fail(rel, "no <main> element")

        # Words in <main>.
        if not original and not index:
            words = len(" ".join(p.main_text).split())
            lo, hi = WORD_EXCEPTIONS.get(rel, DEFAULT_WORDS)
            if not lo <= words <= hi:
                fail(rel, f"<main> has {words} words (allowed {lo} to {hi})")

        # Inline styles and scripts.
        if p.inline_style_tags:
            fail(rel, f"{p.inline_style_tags} inline <style> element(s)")
        if p.style_attrs:
            fail(rel, f"{p.style_attrs} style= attribute(s)")
        if p.bad_scripts:
            fail(rel, f"{p.bad_scripts} inline <script> without src that is not JSON-LD")

        # Subresources from other hosts.
        errors.extend(check_subresources(rel, p.subresources))

        # Links and sources.
        for tag, attr, value in p.refs:
            v = value.strip()
            if not v or re.match(r"^(https?:|mailto:|tel:|data:|//)", v):
                if v.startswith(BASE + "/"):
                    target = url_to_page(v)
                    if target and not (target in page_set or exists(target)):
                        fail(rel, f"{tag} {attr}={v!r} points at a missing file")
                continue
            if v.startswith("#"):
                if len(v) > 1 and v[1:] not in p.ids:
                    fail(rel, f"in-page anchor {v!r} has no matching id")
                continue
            if not v.startswith("/") and depth > 0:
                fail(rel, f"relative {attr}={v!r} in a subdirectory page (use a root-absolute path)")
                continue
            path = v
            frag = None
            path_only = path.split("#")[0].split("?")[0]
            if "#" in path:
                frag = path.split("#", 1)[1]
            if path.startswith("/"):
                fs = path_only.lstrip("/")
            else:
                fs = os.path.normpath(os.path.join(os.path.dirname(rel), path_only)).replace(os.sep, "/")
            if fs == "" or fs.endswith("/"):
                fs += "index.html"
            if fs.endswith(".md"):
                if fs[: -len(".md")] not in page_set and fs != "llms-full.txt":
                    fail(rel, f"{tag} {attr}={v!r}: no page for this markdown mirror")
                continue
            if not exists(fs):
                fail(rel, f"{tag} {attr}={v!r} does not resolve to a file in the tree")
                continue
            if frag and fs in parsed and frag not in parsed[fs].ids:
                fail(rel, f"{v!r}: anchor #{frag} not found in {fs}")

        # Text rules. Sentences come from the body, every meta content value
        # and every string inside the JSON-LD.
        lines = body_lines(raw)
        ld_strings = []
        for doc in parsed_ld:
            collect_strings(doc, ld_strings)
        errors.extend(check_sentence_rules(
            rel, lines + p.meta_contents + ld_strings))
        errors.extend(check_prices(rel, tag_stripped(raw)))

        # Publisher disclosure on comparison pages.
        if rel.startswith(DISCLOSURE_SECTIONS) and \
                DISCLOSURE not in " ".join(lines):
            fail(rel, f'visible text lacks the publisher disclosure "{DISCLOSURE}"')

    # sitemap.xml
    sitemap_pages = []
    try:
        tree = ET.parse(os.path.join(root, "sitemap.xml"))
        ns = {"s": "http://www.sitemaps.org/schemas/sitemap/0.9"}
        for loc in tree.getroot().findall("s:url/s:loc", ns):
            u = (loc.text or "").strip()
            if u.endswith(".md"):
                fail("sitemap.xml", f".md URL listed: {u}")
                continue
            target = url_to_page(u)
            if target is None or target not in page_set:
                fail("sitemap.xml", f"{u} is not a page in the tree")
                continue
            if u != page_url(target):
                fail("sitemap.xml", f"{u} is not the canonical URL {page_url(target)}")
            sitemap_pages.append(target)
    except (ET.ParseError, OSError) as exc:
        fail("sitemap.xml", f"cannot read: {exc}")
    for rel in pages:
        n = sitemap_pages.count(rel)
        if n == 0:
            fail(rel, "missing from sitemap.xml")
        elif n > 1:
            fail(rel, f"listed {n} times in sitemap.xml")

    # llms.txt
    try:
        llms = read_text(os.path.join(root, "llms.txt"))
    except (OSError, UnicodeDecodeError) as exc:
        llms = ""
        fail("llms.txt", f"cannot read: {exc}")
    linked = set(re.findall(r"\]\((https://muxr\.app/[^)\s]*)\)", llms))
    for rel in pages:
        mirror = f"{BASE}/{rel}.md"
        if mirror not in linked:
            fail(rel, f"markdown mirror {mirror} is not linked from llms.txt")
    for u in sorted(linked):
        path = u[len(BASE) + 1:]
        if path.endswith(".md"):
            if path[: -len(".md")] not in page_set:
                fail("llms.txt", f"{u} has no matching page")
        elif path == "llms-full.txt":
            pass
        elif not exists(path):
            fail("llms.txt", f"{u} does not exist in the tree")
    if PRICE_SENTENCE not in llms:
        fail("llms.txt", "lacks the verbatim price sentence")
    if DISAMBIGUATION not in llms:
        fail("llms.txt", "lacks the verbatim disambiguation sentence")
    errors.extend(check_prices("llms.txt", tag_stripped(llms)))

    # IndexNow key file: one 32-character lowercase hex line; not a page.
    try:
        key = read_text(os.path.join(root, "indexnow-key.txt"))
        if not re.fullmatch(r"[0-9a-f]{32}\n?", key):
            fail("indexnow-key.txt", "must hold exactly one 32-character lowercase hex key")
    except (OSError, UnicodeDecodeError) as exc:
        fail("indexnow-key.txt", f"cannot read: {exc}")

    return errors


def main():
    root = os.path.normpath(os.path.join(os.path.dirname(
        os.path.abspath(__file__)), ".."))
    errors = run(root)
    if errors:
        print(f"check_site: {len(errors)} failure(s)")
        for e in errors:
            print("  FAIL " + e)
        return 1
    print(f"check_site: OK ({len(find_pages(root))} pages, sitemap, llms.txt, key file)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
