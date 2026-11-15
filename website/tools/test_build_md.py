"""Unit tests for build_md.py.

Run from website/:

    python3 -m unittest discover -s tools -p 'test_*.py'

Every fixture is written to a temporary directory; nothing is written inside
the repository.
"""

import io
import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout

import build_md

BASE = "https://muxr.app"


def page(main, title="Fixture page · Muxr", head="", body_extra=""):
    """A page shaped like the site's: shared nav and footer around <main>."""
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <title>{title}</title>
  <meta name="description" content="A fixture page.">
  {head}
</head>
<body>
  <header class="site-nav"><a href="/index.html">Home nav link</a></header>
  {body_extra}
  <main class="article">
{main}
  </main>
  <footer class="site-footer"><a href="/terms.html">Footer terms</a></footer>
  <script src="/assets/js/main.js"></script>
</body>
</html>
"""


class SiteDir:
    """A temporary site tree with a sitemap, written file by file."""

    def __init__(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = os.path.join(self._tmp.name, "site")
        self.out = os.path.join(self._tmp.name, "out")
        os.makedirs(self.root)

    def write(self, rel, text):
        path = os.path.join(self.root, *rel.split("/"))
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8", newline="\n") as fh:
            fh.write(text)
        return path

    def sitemap(self, *rels):
        locs = "".join(f"<url><loc>{BASE}/{r}</loc></url>" for r in rels)
        self.write("sitemap.xml",
                   '<?xml version="1.0" encoding="UTF-8"?>'
                   '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">'
                   + locs + "</urlset>")

    def cleanup(self):
        self._tmp.cleanup()


class ConvertTest(unittest.TestCase):
    """convert_page() on one page."""

    def setUp(self):
        self.site = SiteDir()
        self.addCleanup(self.site.cleanup)

    def convert(self, main, rel="how-to/fixture.html", **kwargs):
        self.site.write(rel, page(main, **kwargs))
        return build_md.convert_page(self.site.root, rel, BASE)

    def test_header_title_description_and_source(self):
        md = self.convert("<h1>Fixture page</h1><p>Body.</p>",
                          head='<link rel="canonical" href="https://muxr.app/how-to/fixture.html">')
        self.assertTrue(md.startswith(
            "# Fixture page\n\n> A fixture page.\n\n"
            "Source: https://muxr.app/how-to/fixture.html\n\n"))
        # The page's own h1 repeats the title and is not written twice.
        self.assertEqual(md.count("# Fixture page"), 1)

    def test_headings(self):
        md = self.convert("<h2>Does it work?</h2><h3>Second level</h3>"
                          "<h4>Third level</h4>")
        self.assertIn("\n## Does it work?\n", md)
        self.assertIn("\n### Second level\n", md)
        self.assertIn("\n#### Third level\n", md)

    def test_paragraphs(self):
        md = self.convert("<p>First paragraph\n   wraps here.</p>"
                          "<p>Second paragraph.</p>")
        self.assertIn("\n\nFirst paragraph wraps here.\n\nSecond paragraph.\n", md)

    def test_paragraph_cannot_open_as_a_heading(self):
        md = self.convert("<p># not a heading</p>")
        self.assertIn("\n\\# not a heading\n", md)

    def test_nested_lists(self):
        md = self.convert("<ul><li>One<ul><li>One A</li><li>One B</li></ul></li>"
                          "<li>Two</li></ul><ol start=\"3\"><li>Third</li>"
                          "<li>Fourth</li></ol>")
        self.assertIn("- One\n  - One A\n  - One B\n- Two", md)
        self.assertIn("3. Third\n4. Fourth", md)

    def test_table_with_header_cells(self):
        md = self.convert(
            '<table class="compare-table"><caption>Plans</caption>'
            '<thead><tr><th scope="col">Name</th><th scope="col">Price</th></tr></thead>'
            '<tbody><tr><th scope="row">Free</th><td>None</td></tr>'
            '<tr><th scope="row">Pro</th><td>A | B</td></tr></tbody></table>')
        self.assertIn("*Plans*\n\n| Name | Price |\n|---|---|\n"
                      "| Free | None |\n| Pro | A \\| B |", md)

    def test_table_header_row_without_thead(self):
        md = self.convert("<table><tr><th>Name</th><th>Price</th></tr>"
                          "<tr><td>Free</td><td>None</td></tr></table>")
        self.assertIn("| Name | Price |\n|---|---|\n| Free | None |", md)

    def test_table_without_header_cells(self):
        md = self.convert("<table><tr><td>a</td><td>b</td></tr>"
                          "<tr><td>c</td><td>d</td></tr></table>")
        self.assertIn("|  |  |\n|---|---|\n| a | b |\n| c | d |", md)

    def test_fenced_code_block(self):
        md = self.convert('<pre><code class="language-sh">cargo build\n'
                          '  muxrd serve --port 8443</code></pre>')
        self.assertIn("```sh\ncargo build\n  muxrd serve --port 8443\n```", md)

    def test_inline_code_and_emphasis(self):
        md = self.convert("<p>Run <code>muxrd --help</code>, then <strong>pair</strong> "
                          "with <em>care</em> and <b>bold</b> <i>italic</i>.</p>")
        self.assertIn("Run `muxrd --help`, then **pair** with *care* and "
                      "**bold** *italic*.", md)

    def test_text_markdown_characters_are_escaped(self):
        md = self.convert("<p>Use [brackets] and *stars*.</p>")
        self.assertIn("Use \\[brackets\\] and \\*stars\\*.", md)

    def test_links_made_absolute(self):
        md = self.convert('<p><a href="other.html">sibling</a>, '
                          '<a href="../faq.html">parent</a>, '
                          '<a href="/docs.html#start">root</a>, '
                          '<a href="https://example.com/x">external</a>.</p>')
        self.assertIn("[sibling](https://muxr.app/how-to/other.html)", md)
        self.assertIn("[parent](https://muxr.app/faq.html)", md)
        self.assertIn("[root](https://muxr.app/docs.html#start)", md)
        self.assertIn("[external](https://example.com/x)", md)

    def test_nav_footer_and_script_dropped(self):
        md = self.convert('<nav class="breadcrumb"><ol><li>Crumb</li></ol></nav>'
                          "<p>Kept text.</p><script>var dropped = 1;</script>"
                          '<button class="copy-btn">Copy</button>')
        self.assertIn("Kept text.", md)
        for gone in ("Crumb", "dropped", "Copy", "Home nav link",
                     "Footer terms", "main.js"):
            self.assertNotIn(gone, md)

    def test_body_fallback_strips_site_nav_and_footer(self):
        self.site.write("plain.html", """<!DOCTYPE html><html><head>
<title>Plain</title></head><body>
<header class="site-nav"><a href="/index.html">Home nav link</a></header>
<p>Body text.</p>
<footer class="site-footer"><a href="/terms.html">Footer terms</a></footer>
</body></html>""")
        md = build_md.convert_page(self.site.root, "plain.html", BASE)
        self.assertIn("Body text.", md)
        self.assertNotIn("Home nav link", md)
        self.assertNotIn("Footer terms", md)

    def test_coming_soon_badge(self):
        md = self.convert('<p>Desktop apps <span class="soon">Coming soon</span></p>'
                          '<table><thead><tr><th>Item</th><th>State</th></tr></thead>'
                          '<tbody><tr><td>tmux</td><td><span class="soon">Coming soon</span>'
                          '</td></tr></tbody></table>')
        self.assertIn("Desktop apps *Coming soon*", md)
        self.assertIn("| tmux | *Coming soon* |", md)

    def test_unreadable_page_is_a_named_error(self):
        path = self.site.write("bad.html", "")
        with open(path, "wb") as fh:
            fh.write(b"<p>\xff\xfe not utf-8</p>")
        with self.assertRaises(build_md.BuildError) as ctx:
            build_md.convert_page(self.site.root, "bad.html", BASE)
        self.assertIn("bad.html", str(ctx.exception))

    def test_missing_page_is_a_named_error(self):
        with self.assertRaises(build_md.BuildError) as ctx:
            build_md.convert_page(self.site.root, "missing.html", BASE)
        self.assertIn("missing.html", str(ctx.exception))


class BuildTest(unittest.TestCase):
    """build() and main() over a whole tree."""

    def setUp(self):
        self.site = SiteDir()
        self.addCleanup(self.site.cleanup)
        self.site.write("index.html", page("<h1>Home</h1><p>Welcome.</p>",
                                           title="Muxr"))
        self.site.write("how-to/index.html", page("<h1>How-to</h1><p>Guides.</p>",
                                                  title="How-to · Muxr"))
        self.site.write("how-to/guide.html", page("<h1>Guide</h1><p>Steps.</p>",
                                                  title="Guide · Muxr"))
        self.site.write("assets/skip.html", page("<p>Not a page.</p>"))
        self.site.write(".hidden/skip.html", page("<p>Not a page.</p>"))
        self.site.sitemap("", "how-to/", "how-to/guide.html")

    def read_tree(self, root):
        files = {}
        for dirpath, _, names in os.walk(root):
            for name in names:
                path = os.path.join(dirpath, name)
                with open(path, "rb") as fh:
                    files[os.path.relpath(path, root)] = fh.read()
        return files

    def test_writes_mirrors_and_llms_full(self):
        count = build_md.build(self.site.root, self.site.out, BASE, "2026-11-15")
        self.assertEqual(count, 3)
        files = self.read_tree(self.site.out)
        self.assertEqual(sorted(files), sorted([
            "index.html.md", os.path.join("how-to", "index.html.md"),
            os.path.join("how-to", "guide.html.md"), "llms-full.txt"]))
        full = files["llms-full.txt"].decode("utf-8")
        self.assertIn("generated 2026-11-15", full)
        self.assertLess(full.index("# How-to"), full.index("# Guide"))

    def test_skips_the_same_directories_as_check_site(self):
        import check_site
        pages = sorted(build_md.find_pages(self.site.root, self.site.out))
        self.assertEqual(pages, check_site.find_pages(self.site.root))

    def test_deterministic(self):
        out_a = os.path.join(self.site.out, "a")
        out_b = os.path.join(self.site.out, "b")
        build_md.build(self.site.root, out_a, BASE, "2026-11-15")
        build_md.build(self.site.root, out_b, BASE, "2026-11-15")
        self.assertEqual(self.read_tree(out_a), self.read_tree(out_b))

    def test_source_date_epoch(self):
        saved = os.environ.get("SOURCE_DATE_EPOCH")
        os.environ["SOURCE_DATE_EPOCH"] = "1763164800"  # 2025-11-15 UTC
        try:
            self.assertEqual(build_md.header_date(None), "2025-11-15")
        finally:
            if saved is None:
                del os.environ["SOURCE_DATE_EPOCH"]
            else:
                os.environ["SOURCE_DATE_EPOCH"] = saved

    def test_sitemap_entry_without_page_fails(self):
        self.site.sitemap("", "missing.html")
        with self.assertRaises(build_md.BuildError):
            build_md.build(self.site.root, self.site.out, BASE, "2026-11-15")

    def test_main_reports_unreadable_page_without_traceback(self):
        with open(os.path.join(self.site.root, "how-to", "guide.html"), "wb") as fh:
            fh.write(b"\xff\xfe")
        err = io.StringIO()
        with redirect_stdout(io.StringIO()), redirect_stderr(err):
            code = build_md.main(["--src", self.site.root, "--out", self.site.out,
                                  "--date", "2026-11-15"])
        self.assertEqual(code, 1)
        self.assertIn("build_md: error: how-to/guide.html: cannot read page", err.getvalue())
        self.assertNotIn("Traceback", err.getvalue())


if __name__ == "__main__":
    unittest.main()
