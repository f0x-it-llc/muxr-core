#!/usr/bin/env python3
"""Check the muxr.app static site against website/PAGE_RULES.md.

Run from website/:

    python3 tools/check_site.py

Prints every failure and exits 1, or prints a summary and exits 0. Standard
library only. Pages are enumerated from disk (every *.html below the site
root, minus assets/fonts/tools), never from a hard-coded list, apart from the
seven original pages that keep their own layout and are exempt from the
new-page rules.
"""

import json
import os
import re
import sys
import xml.etree.ElementTree as ET
from html import unescape
from html.parser import HTMLParser

BASE = "https://muxr.app"

# Directories under the site root that never hold pages.
SKIP_DIRS = {"assets", "fonts", "tools", "node_modules"}

# The seven original pages: own layout, relative paths allowed, exempt from the
# new-page path, word-count, title-suffix and JSON-LD-type rules.
ORIGINAL_PAGES = {
    "index.html", "docs.html", "advanced.html", "guide.html",
    "support.html", "privacy.html", "terms.html",
}

# Hosting-vendor and platform names that must not appear anywhere in the tree.
VENDOR_NAMES = [
    "cloudflare", "dokploy", "traefik", "vercel", "netlify", "heroku",
    "github pages", "fly.io", "digitalocean", "hetzner",
]
# This script holds the list above, so it is not scanned for them.
VENDOR_SCAN_SKIP = {"tools/check_site.py"}
TEXT_SUFFIXES = (".html", ".md", ".txt", ".xml", ".css", ".js", ".sh", ".py",
                 ".conf", ".json", ".svg")

# Word limits for <main> of new pages (PAGE_RULES.md section 5).
DEFAULT_WORDS = (400, 900)
WORD_EXCEPTIONS = {"about.html": (300, 500), "faq.html": (400, 1800)}

PRICE_SENTENCE = ("Muxr Pro is a one-time $19.99 purchase, "
                  "50% off ($9.99) until January 13, 2027.")
DISAMBIGUATION = ("Muxr by F0X IT LLC (muxr.app) is a zellij and herdr client "
                  "and is unrelated to the npm package @trymuxr/cli at "
                  "trymuxr.com.")

# Names of other products. A "9.99" figure outside Muxr's own price sentence
# is accepted only when one of these appears near it (another product's price).
OTHER_PRODUCTS = [
    "moshi", "termius", "blink", "prompt 3", "shellfish", "zelmux", "panic",
    "remote control", "herdr mobile", "app store", "website:",
]

# Narrow exceptions to the "open source" rule: sentences that call Blink Shell
# open source (CLAIMS.md records it as such, with its source link) and merely
# name Muxr by contrast. Keyed by page; the regex must match the sentence start.
OPEN_SOURCE_EXCEPTIONS = {
    "compare/blink-shell.html": r"^Blink Shell is a general SSH and Mosh terminal",
    "compare/index.html": r"^An open-source SSH and Mosh terminal for iOS against Muxr\.$",
}

errors = []


def fail(page, msg):
    errors.append(f"{page}: {msg}")


class PageParser(HTMLParser):
    """Collects what the checks need from one HTML page."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.titles = []
        self.in_title = False
        self.meta = {}          # name/property -> content
        self.links = []         # (rel, type, href)
        self.refs = []          # (tag, attr, value) for href/src
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
        a = dict(attrs)
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
        elif tag == "link":
            self.links.append((a.get("rel", ""), a.get("type", ""),
                               a.get("href", "")))
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
        if tag == "main":
            self.has_main = True
        if self.main_depth and tag not in ("script", "style"):
            pass
        if tag == "main":
            self.main_depth += 1
        for attr in ("href", "src"):
            if attr in a and a[attr] is not None:
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


def tag_stripped(raw):
    """Plain text of a page (JSON-LD text included), whitespace-normalised."""
    text = re.sub(r"<[^>]+>", " ", raw)
    return re.sub(r"\s+", " ", unescape(text)).strip()


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


def visible_text(raw):
    """Sentences a reader sees: body text plus meta descriptions, one block
    element per line so table cells and headings do not run together."""
    body = re.search(r"<body.*?</body>", raw, re.S | re.I)
    body = body.group(0) if body else raw
    body = re.sub(r"<(script|style)\b.*?</\1>", " ", body, flags=re.S | re.I)
    body = re.sub(r"<span class=\"hub-card[^>]*>", "\n", body)
    body = re.sub(r"</?(td|th|tr|li|p|h[1-6]|div|dd|dt|caption|section|nav|"
                  r"header|footer|main|ul|ol|table|br|a)\b[^>]*>", "\n", body)
    lines = [unescape(re.sub(r"<[^>]+>", " ", ln)) for ln in body.split("\n")]
    metas = [unescape(m) for m in re.findall(
        r'<meta[^>]+content="([^"]*)"', raw)]
    return [re.sub(r"\s+", " ", ln).strip() for ln in lines + metas]


def find_pages(root):
    pages = []
    for dirpath, dirs, files in os.walk(root):
        rel_dir = os.path.relpath(dirpath, root)
        if rel_dir == ".":
            dirs[:] = sorted(d for d in dirs if d not in SKIP_DIRS)
        else:
            dirs[:] = sorted(dirs)
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


def check_sentence_rules(rel, lines):
    # No "open source" in the same sentence as Muxr or muxrd.
    sentences = []
    for ln in lines:
        sentences.extend(re.split(r"(?<=[.!?])\s+", ln))
    exempt = OPEN_SOURCE_EXCEPTIONS.get(rel)
    for sentence in sentences:
        if exempt and re.search(exempt, sentence):
            continue
        if (re.search(r"open[\s\-]source", sentence, re.I)
                and re.search(r"\bMuxr\b|\bmuxrd\b", sentence)):
            fail(rel, f'"open source" in a sentence naming Muxr/muxrd: '
                      f'{sentence[:160]!r}')


def check_prices(rel, text):
    """Test Muxr's own price statements, not every "9.99" on the page.

    Muxr's price may only appear as the verbatim sentence
    "Muxr Pro is a one-time $19.99 purchase, 50% off ($9.99) until January
    13, 2027." Pages legitimately also carry other products' prices (Moshi
    "$9.99", Blink "$19.99/year"), so the rule works on what is left after
    removing every verbatim sentence:
      * "Muxr Pro" must not be followed by a dollar amount or sale claim,
      * "50% off" and "January 13, 2027" must not appear,
      * a "9.99" must have another product's name within 300 characters.
    When the sale ends, remove the clause from the sentence everywhere and
    update PRICE_SENTENCE here.
    """
    rest = text.replace(PRICE_SENTENCE, " ")
    for m in re.finditer(r"Muxr Pro", rest):
        window = rest[m.end(): m.end() + 100]
        if re.search(r"\$\d|50% off|January 13", window):
            fail(rel, "Muxr Pro price stated outside the verbatim sentence: "
                      f"{rest[m.start(): m.end() + 100]!r}")
    for needle in ("50% off", "January 13, 2027"):
        if needle in rest:
            fail(rel, f'"{needle}" outside the verbatim price sentence')
    for m in re.finditer(r"9\.99", rest):
        window = rest[max(0, m.start() - 300): m.end() + 300].lower()
        if not any(name in window for name in OTHER_PRODUCTS):
            fail(rel, "9.99 outside the verbatim price sentence and with no "
                      f"other product named nearby: {rest[max(0, m.start() - 60): m.end() + 40]!r}")
    if rel == "pricing.html" and PRICE_SENTENCE not in text:
        fail(rel, "pricing.html lacks the verbatim price sentence")


def main():
    root = os.path.normpath(os.path.join(os.path.dirname(
        os.path.abspath(__file__)), ".."))
    os.chdir(root)

    pages = find_pages(root)
    page_set = set(pages)
    parsed = {}
    raws = {}
    for rel in pages:
        with open(rel, encoding="utf-8") as fh:
            raws[rel] = fh.read()
        p = PageParser()
        p.feed(raws[rel])
        p.close()
        parsed[rel] = p

    titles_seen = {}

    for rel in pages:
        p = parsed[rel]
        raw = raws[rel]
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

        # Links and sources.
        for tag, attr, value in p.refs:
            if tag == "link" and attr == "href" and value.startswith(BASE):
                pass  # canonical; already checked
            v = value.strip()
            if not v or re.match(r"^(https?:|mailto:|tel:|data:|//)", v):
                if v.startswith(BASE + "/"):
                    target = url_to_page(v)
                    if target and not (target in page_set or os.path.isfile(target)):
                        fail(rel, f"{tag} {attr}={v!r} points at a missing file")
                continue
            if v.startswith("#"):
                if len(v) > 1 and v[1:] not in p.ids:
                    fail(rel, f"in-page anchor {v!r} has no matching id")
                continue
            if not v.startswith("/"):
                if depth > 0:
                    fail(rel, f"relative {attr}={v!r} in a subdirectory page (use a root-absolute path)")
                    continue
                path = v
            else:
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
            if not os.path.isfile(fs):
                fail(rel, f"{tag} {attr}={v!r} does not resolve to a file in the tree")
                continue
            if frag and fs in parsed and frag not in parsed[fs].ids:
                fail(rel, f"{v!r}: anchor #{frag} not found in {fs}")

        # Text rules.
        text = tag_stripped(raw)
        check_sentence_rules(rel, visible_text(raw))
        check_prices(rel, text)

    # sitemap.xml
    sitemap_pages = []
    try:
        tree = ET.parse("sitemap.xml")
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
        with open("llms.txt", encoding="utf-8") as fh:
            llms = fh.read()
    except OSError as exc:
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
        elif not os.path.isfile(path):
            fail("llms.txt", f"{u} does not exist in the tree")
    if PRICE_SENTENCE not in llms:
        fail("llms.txt", "lacks the verbatim price sentence")
    if DISAMBIGUATION not in llms:
        fail("llms.txt", "lacks the verbatim disambiguation sentence")
    check_prices("llms.txt", tag_stripped(llms))

    # IndexNow key file: one 32-character lowercase hex line; not a page.
    try:
        with open("indexnow-key.txt", encoding="utf-8") as fh:
            key = fh.read()
        if not re.fullmatch(r"[0-9a-f]{32}\n?", key):
            fail("indexnow-key.txt", "must hold exactly one 32-character lowercase hex key")
    except OSError as exc:
        fail("indexnow-key.txt", f"cannot read: {exc}")

    # Hosting-vendor names anywhere under website/.
    for dirpath, dirs, files in os.walk(root):
        rel_dir = os.path.relpath(dirpath, root)
        if rel_dir == ".":
            dirs[:] = [d for d in dirs if d not in ("fonts", "node_modules")]
        for f in files:
            path = os.path.join(rel_dir, f) if rel_dir != "." else f
            path = path.replace(os.sep, "/")
            if path in VENDOR_SCAN_SKIP:
                continue
            if not (f.endswith(TEXT_SUFFIXES) or f in ("Dockerfile",)):
                continue
            try:
                with open(path, encoding="utf-8") as fh:
                    content = fh.read().lower()
            except (OSError, UnicodeDecodeError):
                continue
            for name in VENDOR_NAMES:
                if re.search(r"\b" + re.escape(name) + r"\b", content):
                    fail(path, f"hosting-vendor name {name!r}")

    if errors:
        print(f"check_site: {len(errors)} failure(s)")
        for e in errors:
            print("  FAIL " + e)
        return 1
    print(f"check_site: OK ({len(pages)} pages, sitemap, llms.txt, key file)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
