"""Unit tests for check_site.py.

Run from website/:

    python3 -m unittest discover -s tools -p 'test_*.py'

Each test builds a small site in a temporary directory: an index page, one
comparison page that follows PAGE_RULES.md, a sitemap, llms.txt and the
IndexNow key. The good tree must pass; every test then breaks one rule and
expects check_site.run() to report it. Nothing is written inside the
repository.
"""

import json
import os
import tempfile
import unittest

import check_site

BASE = "https://muxr.app"
PAGE = "compare/blink-shell.html"

FONT_LINKS = """
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=Space+Mono&display=swap" rel="stylesheet">"""

# 450 words of neutral body text, inside the 400 to 900 word range.
FILLER = " ".join(["Plain words fill this fixture page."] * 75)


def article_ld(rel, description="A fixture comparison page."):
    url = BASE + "/" + rel
    return {
        "@context": "https://schema.org",
        "@graph": [
            {"@type": "Article", "headline": "Fixture comparison",
             "description": description,
             "mainEntityOfPage": {"@type": "WebPage", "@id": url}},
            {"@type": "BreadcrumbList", "itemListElement": [
                {"@type": "ListItem", "position": 1, "name": "Home",
                 "item": BASE + "/"}]},
        ],
    }


def new_page(rel=PAGE, title="Fixture comparison · Muxr", head="", main="",
             ld=None, disclosure=True, description="A fixture comparison page."):
    """A new page that passes every rule unless a parameter breaks one."""
    url = BASE + "/" + rel
    if ld is None:
        ld = json.dumps(article_ld(rel, description), indent=2)
    disclosure_p = ("<p>This page is published by the makers of Muxr.</p>"
                    if disclosure else "")
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <title>{title}</title>
  <meta name="description" content="{description}">
  <link rel="canonical" href="{url}">
  <link rel="alternate" type="text/markdown" href="/{rel}.md">
  <meta property="og:url" content="{url}">{FONT_LINKS}
  <link rel="stylesheet" href="/assets/css/glyph.css?v=4">
  {head}
  <script type="application/ld+json">
{ld}
  </script>
</head>
<body>
  <header class="site-nav"><a href="/index.html">Home</a></header>
  <main class="article">
    <h1>Fixture comparison</h1>
    <p>{FILLER}</p>
    <p>See <a href="https://example.com/" target="_blank" rel="noopener">another site</a>.</p>
    {disclosure_p}
    {main}
  </main>
  <footer class="site-footer"><a href="/index.html">Home</a></footer>
  <script src="/assets/js/main.js"></script>
</body>
</html>
"""


INDEX_PAGE = f"""<!DOCTYPE html>
<html lang="en">
<head>
  <title>Muxr</title>
  <meta name="description" content="The fixture home page.">
  <link rel="canonical" href="{BASE}/">
  <link rel="alternate" type="text/markdown" href="/index.html.md">{FONT_LINKS}
  <script type="application/ld+json">{{"@context": "https://schema.org", "@type": "WebSite"}}</script>
</head>
<body><main><h1>Muxr</h1><p>Home.</p></main></body>
</html>
"""


class Site:
    """A temporary site tree that passes check_site.run() as built."""

    def __init__(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = self._tmp.name
        self.pages = {"index.html": INDEX_PAGE, PAGE: new_page()}
        self.write("assets/css/glyph.css", "body {}\n")
        self.write("assets/js/main.js", "\n")
        self.write("indexnow-key.txt", "0123456789abcdef0123456789abcdef\n")
        self.save()

    def write(self, rel, text):
        path = os.path.join(self.root, *rel.split("/"))
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8", newline="\n") as fh:
            fh.write(text)

    def save(self, sitemap=None, llms=None):
        """Write every page, then a sitemap and llms.txt listing them."""
        for rel, text in self.pages.items():
            self.write(rel, text)
        listed = sorted(self.pages) if sitemap is None else sitemap
        self.write("sitemap.xml",
                   '<?xml version="1.0" encoding="UTF-8"?>\n'
                   '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n'
                   + "".join(f"  <url><loc>{check_site.page_url(r)}</loc></url>\n"
                             for r in listed)
                   + "</urlset>\n")
        mirrors = sorted(self.pages) if llms is None else llms
        self.write("llms.txt",
                   "# Muxr\n\n" + check_site.PRICE_SENTENCE + "\n\n"
                   + check_site.DISAMBIGUATION + "\n\n"
                   + "".join(f"- [{r}]({BASE}/{r}.md)\n" for r in mirrors))

    def cleanup(self):
        self._tmp.cleanup()


class CheckSiteTest(unittest.TestCase):

    def setUp(self):
        self.site = Site()
        self.addCleanup(self.site.cleanup)

    def run_with_page(self, **kwargs):
        self.site.pages[PAGE] = new_page(**kwargs)
        self.site.save()
        return check_site.run(self.site.root)

    def assertFails(self, errors, *needles):
        joined = "\n".join(errors)
        for needle in needles:
            self.assertIn(needle, joined, msg="expected a failure containing "
                          f"{needle!r}, got:\n{joined or '(no failures)'}")

    # The good tree ---------------------------------------------------------

    def test_good_site_passes(self):
        self.assertEqual(check_site.run(self.site.root), [])

    # Links, sitemap, llms.txt ---------------------------------------------

    def test_broken_internal_link(self):
        errors = self.run_with_page(main='<p><a href="/missing.html">Gone</a>.</p>')
        self.assertFails(errors, "href='/missing.html' does not resolve")

    def test_broken_anchor(self):
        errors = self.run_with_page(main='<p><a href="#nowhere">Jump</a>.</p>')
        self.assertFails(errors, "in-page anchor '#nowhere' has no matching id")

    def test_missing_sitemap_entry(self):
        self.site.save(sitemap=["index.html"])
        self.assertFails(check_site.run(self.site.root),
                         f"{PAGE}: missing from sitemap.xml")

    def test_missing_llms_line(self):
        self.site.save(llms=["index.html"])
        self.assertFails(check_site.run(self.site.root),
                         f"{PAGE}: markdown mirror {BASE}/{PAGE}.md is not linked")

    def test_relative_href_in_subdirectory_page(self):
        errors = self.run_with_page(main='<p><a href="moshi.html">Moshi</a>.</p>')
        self.assertFails(errors, "relative href='moshi.html' in a subdirectory page")

    # "open source" ---------------------------------------------------------

    def test_open_source_in_plain_text(self):
        errors = self.run_with_page(main="<p>Muxr is open source software.</p>")
        self.assertFails(errors, '"open source" in a sentence naming Muxr')

    def test_open_source_inside_a_link(self):
        errors = self.run_with_page(
            main='<p>Muxr is <a href="/index.html">open source</a> software.</p>')
        self.assertFails(errors, "'Muxr is open source software.'")

    def test_open_source_inside_emphasis(self):
        errors = self.run_with_page(
            main="<p>Muxr is <strong>open</strong>-<em>source</em> today.</p>")
        self.assertFails(errors, '"open source" in a sentence naming Muxr')

    def test_open_source_inside_json_ld(self):
        ld = article_ld(PAGE, "A fixture comparison page.")
        ld["@graph"][0]["about"] = {"@type": "Thing",
                                    "name": "Muxr is an open-source app."}
        errors = self.run_with_page(ld=json.dumps(ld))
        self.assertFails(errors, "'Muxr is an open-source app.'")

    def test_open_source_in_single_quoted_meta(self):
        errors = self.run_with_page(
            head="<meta property='og:description' content='Muxr is open source.'>")
        self.assertFails(errors, "'Muxr is open source.'")

    def test_open_source_exception_is_exact(self):
        allowed = next(iter(check_site.OPEN_SOURCE_EXCEPTIONS[PAGE]))
        self.assertEqual(
            self.run_with_page(main=f"<p>{allowed}</p>"), [])
        errors = self.run_with_page(
            main="<p>" + allowed.replace("Blink Shell is", "Muxr is") + "</p>")
        self.assertFails(errors, '"open source" in a sentence naming Muxr')

    def test_open_source_about_another_product_passes(self):
        self.assertEqual(self.run_with_page(
            main="<p>Blink Shell is open source.</p><p>Muxr is a client.</p>"), [])

    # Prices ----------------------------------------------------------------

    def test_verbatim_price_sentence_passes(self):
        self.assertEqual(self.run_with_page(
            main=f"<p>{check_site.PRICE_SENTENCE}</p>"), [])

    def test_other_product_price_passes(self):
        self.assertEqual(self.run_with_page(
            main="<p>Moshi lists Pro at $9.99 per month on the App Store.</p>"), [])

    def test_wrong_muxr_price_near_app_store(self):
        errors = self.run_with_page(
            main="<p>Muxr costs $9.99 on the App Store, with remote control.</p>")
        self.assertFails(errors, "9.99 outside the verbatim price sentence")

    def test_muxr_pro_price_outside_sentence(self):
        errors = self.run_with_page(main="<p>Muxr Pro costs $14.99 once.</p>")
        self.assertFails(errors, "Muxr Pro price stated outside the verbatim sentence")

    # Styles, scripts, JSON-LD, title --------------------------------------

    def test_inline_style_attribute(self):
        errors = self.run_with_page(main='<p style="color: red">Red.</p>')
        self.assertFails(errors, "1 style= attribute(s)")

    def test_inline_script(self):
        errors = self.run_with_page(main="<script>var x = 1;</script>")
        self.assertFails(errors, "inline <script> without src")

    def test_unparseable_json_ld(self):
        errors = self.run_with_page(ld='{"@context": "https://schema.org",}')
        self.assertFails(errors, "JSON-LD does not parse")

    def test_title_over_60_characters(self):
        errors = self.run_with_page(title="A" * 55 + " · Muxr")
        self.assertFails(errors, "title is 62 characters (max 60)")

    # External subresources -------------------------------------------------

    def test_third_party_script(self):
        errors = self.run_with_page(
            head='<script src="https://scripts.example.net/tag.js"></script>')
        self.assertFails(errors, "<script> loads 'https://scripts.example.net/tag.js' "
                                 "from external host scripts.example.net")

    def test_third_party_stylesheet(self):
        errors = self.run_with_page(
            head='<link rel="stylesheet" href="https://styles.example.net/a.css">')
        self.assertFails(errors, "<link rel=stylesheet> loads "
                                 "'https://styles.example.net/a.css'")

    def test_protocol_relative_image(self):
        errors = self.run_with_page(main='<img src="//img.example.net/a.png" alt="">')
        self.assertFails(errors, "external host img.example.net")

    def test_font_host_cannot_serve_scripts(self):
        errors = self.run_with_page(
            head='<script src="https://fonts.googleapis.com/x.js"></script>')
        self.assertFails(errors, "external host fonts.googleapis.com")

    def test_other_subresource_elements(self):
        main = ('<iframe src="https://a.example.net/"></iframe>'
                '<video src="https://b.example.net/v.mp4"></video>'
                '<audio src="https://c.example.net/a.mp3"></audio>'
                '<object data="https://d.example.net/o.swf"></object>'
                '<embed src="https://e.example.net/e.swf">'
                '<picture><source srcset="https://f.example.net/p.webp 2x"></picture>')
        errors = self.run_with_page(main=main)
        self.assertFails(errors, *[f"external host {h}.example.net"
                                   for h in "abcdef"])

    def test_external_links_and_data_images_pass(self):
        self.assertEqual(self.run_with_page(
            main='<p><a href="https://other.example.org/page">Other</a>.</p>'
                 '<img src="data:image/png;base64,AAAA" alt="">'
                 f'<img src="{BASE}/assets/css/glyph.css" alt="">'), [])

    # Publisher disclosure -------------------------------------------------

    def test_comparison_page_without_disclosure(self):
        errors = self.run_with_page(disclosure=False)
        self.assertFails(errors, f'{PAGE}: visible text lacks the publisher '
                                 'disclosure "makers of Muxr"')

    def test_disclosure_only_in_meta_does_not_count(self):
        errors = self.run_with_page(
            disclosure=False,
            description="Published by the makers of Muxr.")
        self.assertFails(errors, "lacks the publisher disclosure")

    def test_disclosure_split_by_an_inline_tag_counts(self):
        self.assertEqual(self.run_with_page(
            disclosure=False,
            main="<p>Published by the makers of <strong>Muxr</strong>.</p>"), [])

    def test_disclosure_not_required_outside_comparison_sections(self):
        rel = "how-to/fixture.html"
        ld = article_ld(rel)
        ld["@graph"][0]["@type"] = "TechArticle"
        self.site.pages[rel] = new_page(rel=rel, title="Fixture how-to · Muxr",
                                        ld=json.dumps(ld), disclosure=False)
        self.site.save()
        self.assertEqual(check_site.run(self.site.root), [])

    # Page enumeration ------------------------------------------------------

    def test_dot_and_skip_directories_hold_no_pages(self):
        self.site.write(".cache/stray.html", "<p>Not a page.</p>")
        self.site.write("compare/assets/stray.html", "<p>Not a page.</p>")
        self.assertEqual(check_site.find_pages(self.site.root),
                         sorted(["index.html", PAGE]))
        self.assertEqual(check_site.run(self.site.root), [])


if __name__ == "__main__":
    unittest.main()
