#!/usr/bin/env python3
"""Generate markdown mirrors of the muxr.app pages and llms-full.txt.

Each page <path>.html becomes <out>/<path>.html.md (the URL every page
advertises with <link rel="alternate" type="text/markdown">), and
<out>/llms-full.txt holds every page's markdown in one file. The mirrors are
built when the website image is built and are never committed.

Usage:
    python3 tools/build_md.py --src . --out <dir> [--base https://muxr.app]
                              [--date YYYY-MM-DD]

Standard library only. The output is deterministic: pages are processed in a
fixed order and the only date written is the llms-full.txt header date
(--date, else SOURCE_DATE_EPOCH, else today in UTC).
"""

import argparse
import datetime
import os
import re
import sys
import xml.etree.ElementTree as ET
from html.parser import HTMLParser
from urllib.parse import urljoin, urlparse

SITE_NAME = "Muxr"
TITLE_SUFFIX = re.compile(r"\s+\u00b7\s+Muxr\s*$")

# Directories under --src that never hold pages, at any depth. Directories
# whose name starts with "." are skipped as well. tools/check_site.py uses the
# same rules, so both tools see the same pages.
SKIP_DIRS = {"assets", "fonts", "tools", "node_modules"}

VOID_TAGS = {
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link",
    "meta", "param", "source", "track", "wbr",
}

# Elements whose whole subtree is left out of the markdown.
DROP_TAGS = {
    "script", "style", "noscript", "template", "svg", "button", "iframe",
    "form", "canvas", "video", "audio", "object", "select", "textarea",
}

# Decorative or navigational blocks inside <main> that carry no content:
# the doc pages' "On this page" sidebar, section dividers and eyebrow
# labels, the fake terminal title bar and the JS-injected copy buttons.
DROP_CLASSES = {
    "copy-btn", "doc-sidebar", "section-divider", "eyebrow",
    "terminal-titlebar", "cursor-block", "nav-toggle",
}

# Blocks rendered as fenced code because the stylesheet preserves their
# whitespace like <pre>.
PRE_CLASSES = {"ascii-box", "terminal-body"}

# Boxed notes rendered as blockquotes; their title line becomes bold.
QUOTE_CLASSES = {"callout", "verdict"}
TITLE_CLASSES = {"callout-title", "verdict-title", "page-links-label"}

# Inline badges rendered in italics so they read apart from the sentence
# they follow ("desktop apps *Coming soon*").
EMPHASIS_CLASSES = {"soon"}

BLOCK_TAGS = {
    "address", "article", "aside", "blockquote", "body", "caption", "dd",
    "details", "div", "dl", "dt", "fieldset", "figcaption", "figure",
    "footer", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "li",
    "main", "nav", "ol", "p", "pre", "section", "summary", "table", "tbody",
    "td", "tfoot", "th", "thead", "tr", "ul",
}

# Start tags that implicitly close an open <p>.
P_CLOSERS = BLOCK_TAGS - {"caption", "dd", "dt", "li", "tbody", "td",
                          "tfoot", "th", "thead", "tr", "body"}

HEADING_TAGS = {"h1": 1, "h2": 2, "h3": 3, "h4": 4, "h5": 5, "h6": 6}


class BuildError(Exception):
    """A page or the sitemap cannot be turned into markdown."""


# --------------------------------------------------------------------------
# A small DOM built with html.parser
# --------------------------------------------------------------------------

class Node:
    __slots__ = ("tag", "attrs", "children", "parent")

    def __init__(self, tag, attrs=None, parent=None):
        self.tag = tag
        self.attrs = attrs or {}
        self.children = []
        self.parent = parent

    @property
    def classes(self):
        return set(self.attrs.get("class", "").split())

    def iter(self):
        yield self
        for child in self.children:
            if isinstance(child, Node):
                yield from child.iter()

    def find(self, tag, cls=None):
        for node in self.iter():
            if node.tag == tag and (cls is None or cls in node.classes):
                return node
        return None

    def text(self):
        parts = []
        for child in self.children:
            parts.append(child.text() if isinstance(child, Node) else child)
        return "".join(parts)


class TreeBuilder(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.root = Node("#document")
        self.stack = [self.root]

    def _close_to(self, index):
        del self.stack[index:]

    def _find_open(self, tag, stop=()):
        """Index of the innermost open `tag`, not looking past `stop` tags."""
        for i in range(len(self.stack) - 1, 0, -1):
            name = self.stack[i].tag
            if name == tag:
                return i
            if name in stop:
                return None
        return None

    def handle_starttag(self, tag, attrs):
        if tag in P_CLOSERS and self.stack[-1].tag == "p":
            self.stack.pop()
        implicit = {
            "li": ("ul", "ol"),
            "dt": ("dl",), "dd": ("dl",),
            "tr": ("table", "thead", "tbody", "tfoot"),
            "td": ("tr", "table"), "th": ("tr", "table"),
            "thead": ("table",), "tbody": ("table",), "tfoot": ("table",),
        }
        if tag in implicit:
            siblings = {"dt", "dd"} if tag in ("dt", "dd") else (
                {"td", "th"} if tag in ("td", "th") else (
                    {"thead", "tbody", "tfoot"}
                    if tag in ("thead", "tbody", "tfoot") else {tag}))
            for sibling in siblings:
                index = self._find_open(sibling, stop=implicit[tag])
                if index is not None:
                    self._close_to(index)
                    break
        attr_map = {}
        for name, value in attrs:
            attr_map[name] = value if value is not None else ""
        node = Node(tag, attr_map, self.stack[-1])
        self.stack[-1].children.append(node)
        if tag not in VOID_TAGS:
            self.stack.append(node)

    def handle_startendtag(self, tag, attrs):
        self.handle_starttag(tag, attrs)
        if tag not in VOID_TAGS and self.stack[-1].tag == tag:
            self.stack.pop()

    def handle_endtag(self, tag):
        if tag in VOID_TAGS:
            return
        index = self._find_open(tag)
        if index is not None:
            self._close_to(index)

    def handle_data(self, data):
        self.stack[-1].children.append(data)


def parse_html(source):
    builder = TreeBuilder()
    builder.feed(source)
    builder.close()
    return builder.root


# --------------------------------------------------------------------------
# HTML to markdown
# --------------------------------------------------------------------------

WS = re.compile(r"[ \t\r\n\f\u00a0]+")
LINE_START_MARKER = re.compile(r"^(#{1,6}(\s|$)|[-+>](\s|$)|\d+[.)](\s|$))")
WORD_EDGE_UNDERSCORE = re.compile(r"(?<![A-Za-z0-9])_|_(?![A-Za-z0-9])")


# Stands in for <br> until whitespace is collapsed, then becomes a markdown
# hard break (backslash, newline).
BR = "\x00"


def one_line(text):
    """Collapse rendered inline markdown to a single trimmed line."""
    return WS.sub(" ", text.replace(BR, " ")).strip()


def escape_line_start(text):
    """Keep a paragraph from opening as a heading, list or quote."""
    match = LINE_START_MARKER.match(text)
    if not match:
        return text
    if text[0].isdigit():
        digits = len(match.group(0).rstrip()) - 1
        return text[:digits] + "\\" + text[digits:]
    return "\\" + text


def escape_text(text):
    text = text.replace("\\", "\\\\")
    for char in "*[]`":
        text = text.replace(char, "\\" + char)
    return WORD_EDGE_UNDERSCORE.sub(r"\\_", text)


def code_span(text):
    text = WS.sub(" ", text).strip()
    if not text:
        return ""
    longest = max((len(m) for m in re.findall(r"`+", text)), default=0)
    fence = "`" * (longest + 1)
    if text.startswith("`") or text.endswith("`"):
        text = " " + text + " "
    return fence + text + fence


def fenced(text, lang=""):
    text = text.replace("\u00a0", " ").replace("\r\n", "\n")
    if text.startswith("\n"):
        text = text[1:]
    text = "\n".join(line.rstrip() for line in text.rstrip().split("\n"))
    longest = max((len(m) for m in re.findall(r"`{3,}", text)), default=0)
    fence = "`" * max(3, longest + 1)
    return fence + lang + "\n" + text + "\n" + fence


def wrap(marker, inner):
    """Wrap inline text in an emphasis marker, keeping edge spaces outside."""
    stripped = inner.strip()
    if not stripped:
        return inner
    lead = " " if inner[:1].isspace() else ""
    trail = " " if inner[-1:].isspace() else ""
    return lead + marker + stripped + marker + trail


def url_escape(url):
    return url.replace(" ", "%20").replace("(", "%28").replace(")", "%29")


class Converter:
    def __init__(self, page_url):
        self.page_url = page_url

    def absolute(self, href):
        href = (href or "").strip()
        if not href:
            return ""
        if href.lower().startswith(("javascript:", "data:")):
            return ""
        return url_escape(urljoin(self.page_url, href))

    # ---- inline ----------------------------------------------------------

    def inline(self, node, in_table=False):
        parts = []
        for child in node.children:
            if isinstance(child, str):
                parts.append(escape_text(WS.sub(" ", child)))
            else:
                parts.append(self.inline_node(child, in_table))
        return "".join(parts)

    def inline_node(self, node, in_table):
        tag = node.tag
        if self.dropped(node):
            return ""
        if tag == "br":
            return "<br>" if in_table else BR
        if tag == "img":
            alt = WS.sub(" ", node.attrs.get("alt", "")).strip()
            src = self.absolute(node.attrs.get("src"))
            if not src:
                return escape_text(alt)
            return "![" + escape_text(alt) + "](" + src + ")"
        if tag in ("code", "kbd", "samp", "tt"):
            return code_span(node.text())
        if tag in ("strong", "b"):
            return wrap("**", self.inline(node, in_table))
        if tag in ("em", "i", "cite", "dfn") or node.classes & EMPHASIS_CLASSES:
            return wrap("*", self.inline(node, in_table))
        if tag in ("s", "del", "strike"):
            return wrap("~~", self.inline(node, in_table))
        if tag == "a":
            return self.link(node, in_table)
        if tag in BLOCK_TAGS:
            return " " + self.inline(node, in_table) + " "
        return self.inline(node, in_table)

    def link(self, node, in_table):
        href = self.absolute(node.attrs.get("href"))
        if "hub-card" in node.classes:
            title = node.find("span", "hub-card-title")
            desc = node.find("span", "hub-card-desc")
            if title is not None and href:
                text = one_line(self.inline(title, in_table))
                out = "[" + text + "](" + href + ")"
                if desc is not None:
                    out += ": " + one_line(self.inline(desc, in_table))
                return out
        inner = self.inline(node, in_table)
        if not href:
            return inner
        text = one_line(inner)
        if not text:
            text = escape_text(href)
        lead = " " if inner[:1].isspace() else ""
        trail = " " if inner[-1:].isspace() else ""
        return lead + "[" + text + "](" + href + ")" + trail

    # ---- blocks ----------------------------------------------------------

    def dropped(self, node):
        if node.tag in DROP_TAGS:
            return True
        if node.tag == "nav" and "page-links" not in node.classes:
            return True
        if node.classes & DROP_CLASSES:
            return True
        return "hidden" in node.attrs

    def is_block(self, node):
        return (node.tag in BLOCK_TAGS or node.classes & PRE_CLASSES
                or node.classes & QUOTE_CLASSES)

    def blocks(self, node):
        """Render a container's children as a list of markdown blocks."""
        out = []
        pending = []

        def flush():
            text = WS.sub(" ", "".join(pending)).strip(BR + " ")
            text = re.sub(" ?" + BR + "[ " + BR + "]*", lambda m: "\\\n", text)
            pending.clear()
            if text:
                out.append(escape_line_start(text))

        for child in node.children:
            if isinstance(child, str):
                pending.append(escape_text(child))
            elif self.dropped(child):
                continue
            elif self.is_block(child):
                flush()
                out.extend(self.block(child))
            else:
                pending.append(self.inline_node(child, False))
        flush()
        return out

    def block(self, node):
        tag = node.tag
        classes = node.classes
        if tag == "pre" or classes & PRE_CLASSES:
            lang = ""
            code = node.find("code")
            if code is not None:
                for cls in code.classes:
                    if cls.startswith("language-"):
                        lang = cls[len("language-"):]
            text = node.text()
            return [fenced(text, lang)] if text.strip() else []
        if tag in HEADING_TAGS:
            text = one_line(self.inline(node))
            if not text:
                return []
            return ["#" * HEADING_TAGS[tag] + " " + text]
        if tag in ("ul", "ol"):
            return self.list_block(node)
        if tag == "table":
            return self.table(node)
        if tag == "hr":
            return ["***"]
        if tag == "blockquote" or classes & QUOTE_CLASSES:
            inner = self.blocks(node)
            if not inner:
                return []
            body = "\n\n".join(inner)
            return ["\n".join("> " + line if line else ">"
                              for line in body.split("\n"))]
        if classes & TITLE_CLASSES or tag in ("dt", "summary"):
            text = one_line(self.inline(node))
            return ["**" + text + "**"] if text else []
        return self.blocks(node)

    def list_block(self, node):
        ordered = node.tag == "ol"
        try:
            number = int(node.attrs.get("start", "1"))
        except ValueError:
            number = 1
        items = []
        for child in node.children:
            if not isinstance(child, Node) or child.tag != "li":
                continue
            if self.dropped(child):
                continue
            marker = (str(number) + ". ") if ordered else "- "
            number += 1
            parts = self.blocks(child)
            if not parts:
                continue
            pad = " " * len(marker)
            lines = []
            for i, part in enumerate(parts):
                # A nested list follows its item's text directly; any other
                # further block is a new paragraph within the item.
                if i > 0 and not re.match(r"^(- |\d+\. )", part):
                    lines.append("")
                for j, line in enumerate(part.split("\n")):
                    if i == 0 and j == 0:
                        lines.append(marker + line)
                    else:
                        lines.append(pad + line if line else "")
            items.append("\n".join(lines))
        return ["\n".join(items)] if items else []

    def table(self, node):
        caption = None
        rows = []
        header_rows = 0
        for section in node.children:
            if not isinstance(section, Node):
                continue
            if section.tag == "caption":
                caption = one_line(self.inline(section))
            elif section.tag in ("thead", "tbody", "tfoot"):
                for row in section.children:
                    if isinstance(row, Node) and row.tag == "tr":
                        rows.append(self.row(row))
                        if section.tag == "thead":
                            header_rows += 1
            elif section.tag == "tr":
                rows.append(self.row(section))
        rows = [r for r in rows if r]
        if not rows:
            return []
        width = max(len(r) for r in rows)
        rows = [r + [""] * (width - len(r)) for r in rows]
        header = rows[0]
        body = rows[1:]
        if header_rows == 0 and len(rows) > 1:
            first = None
            for section in node.iter():
                if section.tag == "tr":
                    first = section
                    break
            cells = [c for c in first.children if isinstance(c, Node)]
            if not all(c.tag == "th" for c in cells):
                header = [""] * width
                body = rows
        lines = ["| " + " | ".join(header) + " |",
                 "|" + "|".join(["---"] * width) + "|"]
        for r in body:
            lines.append("| " + " | ".join(r) + " |")
        out = []
        if caption:
            out.append("*" + caption + "*")
        out.append("\n".join(lines))
        return out

    def row(self, tr):
        cells = []
        for cell in tr.children:
            if not isinstance(cell, Node) or cell.tag not in ("td", "th"):
                continue
            text = one_line(self.inline(cell, in_table=True))
            text = text.replace("|", "\\|")
            cells.append(text)
            try:
                span = int(cell.attrs.get("colspan", "1"))
            except ValueError:
                span = 1
            cells.extend([""] * (max(span, 1) - 1))
        return cells


# --------------------------------------------------------------------------
# Pages
# --------------------------------------------------------------------------

def page_url(base, rel):
    return base + "/" + rel


def default_canonical(base, rel):
    if rel == "index.html":
        return base + "/"
    if rel.endswith("/index.html"):
        return base + "/" + rel[: -len("index.html")]
    return base + "/" + rel


def strip_layout(body):
    """Remove the shared site header and footer from a <body> fallback."""
    def prune(node):
        kept = []
        for child in node.children:
            if isinstance(child, Node):
                if (child.tag == "header" and "site-nav" in child.classes) or \
                        (child.tag == "footer" and "site-footer" in child.classes):
                    continue
                prune(child)
            kept.append(child)
        node.children = kept
    prune(body)
    return body


def convert_page(src_root, rel, base):
    path = os.path.join(src_root, *rel.split("/"))
    try:
        with open(path, encoding="utf-8") as handle:
            source = handle.read()
    except (OSError, UnicodeDecodeError) as err:
        raise BuildError(rel + ": cannot read page: " + str(err))
    doc = parse_html(source)

    title_node = doc.find("title")
    title = WS.sub(" ", title_node.text()).strip() if title_node else ""
    title = TITLE_SUFFIX.sub("", title) or rel

    description = ""
    canonical = ""
    for node in doc.iter():
        if node.tag == "meta" and node.attrs.get("name", "").lower() == "description":
            description = WS.sub(" ", node.attrs.get("content", "")).strip()
        elif node.tag == "link" and "canonical" in node.attrs.get("rel", "").lower().split():
            canonical = node.attrs.get("href", "").strip()
    canonical = canonical or default_canonical(base, rel)

    content = doc.find("main")
    if content is None:
        body = doc.find("body")
        if body is None:
            raise BuildError(rel + ": no <main> and no <body> element")
        content = strip_layout(body)

    converter = Converter(page_url(base, rel))
    blocks = converter.blocks(content)
    # The page's own <h1> usually repeats the title already written on top.
    if blocks and blocks[0].startswith("# ") and \
            blocks[0][2:].strip().lower() == escape_text(title).lower():
        blocks = blocks[1:]
    else:
        for i, block in enumerate(blocks):
            if block.startswith("# ") and \
                    block[2:].strip().lower() == escape_text(title).lower():
                del blocks[i]
                break

    head = ["# " + escape_text(title)]
    if description:
        head.append("> " + escape_text(description))
    head.append("Source: " + canonical)
    return "\n\n".join(head + blocks) + "\n"


def find_pages(src_root, out_root):
    out_abs = os.path.abspath(out_root)
    src_abs = os.path.abspath(src_root)
    pages = []
    for dirpath, dirnames, filenames in os.walk(src_abs):
        rel_dir = os.path.relpath(dirpath, src_abs)
        keep = []
        for name in dirnames:
            full = os.path.join(dirpath, name)
            if name.startswith(".") or name in SKIP_DIRS:
                continue
            if os.path.abspath(full) == out_abs and out_abs != src_abs:
                continue
            keep.append(name)
        dirnames[:] = sorted(keep)
        for name in filenames:
            if name.endswith(".html"):
                rel = name if rel_dir == "." else rel_dir + "/" + name
                pages.append(rel.replace(os.sep, "/"))
    return pages


def path_order(rel):
    """Stable order: by directory, a section's index.html first."""
    parts = rel.split("/")
    return (parts[:-1], 0 if parts[-1] == "index.html" else 1, parts[-1])


def sitemap_pages(src_root):
    path = os.path.join(src_root, "sitemap.xml")
    if not os.path.isfile(path):
        raise BuildError("sitemap.xml not found in " + src_root)
    try:
        tree = ET.parse(path)
    except ET.ParseError as err:
        raise BuildError("sitemap.xml does not parse: " + str(err))
    listed = []
    seen = set()
    for node in tree.iter():
        if node.tag.split("}")[-1] != "loc" or not (node.text or "").strip():
            continue
        loc = node.text.strip()
        rel = urlparse(loc).path.lstrip("/")
        if rel == "" or rel.endswith("/"):
            rel += "index.html"
        if rel not in seen:
            seen.add(rel)
            listed.append((rel, loc))
    return listed


def header_date(arg):
    if arg:
        try:
            return datetime.date.fromisoformat(arg).isoformat()
        except ValueError:
            raise BuildError("--date must be YYYY-MM-DD, got " + repr(arg))
    epoch = os.environ.get("SOURCE_DATE_EPOCH", "").strip()
    if epoch:
        try:
            stamp = datetime.datetime.fromtimestamp(int(epoch), datetime.timezone.utc)
            return stamp.date().isoformat()
        except ValueError:
            raise BuildError("SOURCE_DATE_EPOCH is not an integer: " + repr(epoch))
    return datetime.datetime.now(datetime.timezone.utc).date().isoformat()


def write_file(path, text):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)


def build(src_root, out_root, base, date_arg):
    base = base.rstrip("/")
    date = header_date(date_arg)
    pages = find_pages(src_root, out_root)
    if not pages:
        raise BuildError("no .html pages found under " + src_root)
    on_disk = set(pages)

    order = []
    for rel, loc in sitemap_pages(src_root):
        if rel not in on_disk:
            raise BuildError("sitemap.xml lists " + loc + " but " + rel +
                             " does not exist under " + src_root)
        order.append(rel)
    order += sorted((p for p in pages if p not in order), key=path_order)

    mirrors = {}
    for rel in order:
        markdown = convert_page(src_root, rel, base)
        mirrors[rel] = markdown
        write_file(os.path.join(out_root, *rel.split("/")) + ".md", markdown)

    summary = ""
    if "index.html" in mirrors:
        for line in mirrors["index.html"].split("\n"):
            if line.startswith("> "):
                summary = line
                break
    head = ["# " + SITE_NAME]
    if summary:
        head.append(summary)
    head.append("Every page of " + base + " as markdown, in sitemap order, "
                "generated " + date + ". Each page is also served on its own "
                "at its URL with .md appended, for example " + base +
                "/about.html.md.")
    full = "\n\n".join(head) + "\n"
    for rel in order:
        full += "\n---\n\n" + mirrors[rel]
    write_file(os.path.join(out_root, "llms-full.txt"), full)
    return len(order)


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Generate markdown mirrors of the site's pages and llms-full.txt.")
    parser.add_argument("--src", required=True, help="website source directory")
    parser.add_argument("--out", required=True, help="output directory")
    parser.add_argument("--base", default="https://muxr.app",
                        help="absolute site URL used for links (default: %(default)s)")
    parser.add_argument("--date", help="header date for llms-full.txt, YYYY-MM-DD")
    args = parser.parse_args(argv)
    if not os.path.isdir(args.src):
        print("build_md: --src " + args.src + " is not a directory", file=sys.stderr)
        return 2
    try:
        count = build(args.src, args.out, args.base, args.date)
    except BuildError as err:
        print("build_md: error: " + str(err), file=sys.stderr)
        return 1
    print("build_md: wrote " + str(count) + " markdown mirrors and llms-full.txt to " + args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
