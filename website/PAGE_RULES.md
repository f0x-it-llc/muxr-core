# Page rules

The contract for every new page on muxr.app. `about.html` is the finished
reference implementation: copy its `<head>` and its `.site-nav` and
`.site-footer` unchanged (they already use root-absolute paths), then change
only what this checklist says to change. This file is never served; the
Dockerfile does not copy it into the image.

Work through the list top to bottom. A page that misses an item is not done.

## 1. Facts

- [ ] Every statement about another product comes from `website/CLAIMS.md` and
      links the source URL recorded there. The fetch date ("as published on
      15 November 2026") is visible on the page.
- [ ] A missing fact, or one marked "not stated" or "owner to confirm", is
      written "Not stated" or left out. Never guess. Never fill a gap from
      memory or from another vendor's comparison page.
- [ ] Anything inside a CLAIMS.md block marked DO NOT PUBLISH stays off the page.
- [ ] A CLAIMS.md "Our judgement" line is published in our own voice, never as
      something the other product says about itself.
- [ ] A page that gives a Moshi price shows both sources (website and App
      Store), never a single figure.
- [ ] Muxr facts come from CLAIMS.md under "Muxr".
- [ ] Muxr is "source available". Never write the hyphenated or spaced
      open-source phrase about Muxr. (It is correct for a product CLAIMS.md
      records as open-source, such as Blink Shell, with its source link.)
- [ ] tmux, push notifications, voice input and desktop apps are "coming soon"
      with no date. Use the `.soon` badge in tables.
- [ ] Where the price appears, use this sentence verbatim:
      `Muxr Pro is a one-time $19.99 purchase, 50% off ($9.99) until January 13, 2027.`
- [ ] Where the name could be confused with trymuxr, use the sentence from
      CLAIMS.md "Disambiguation" verbatim.
      It needs no source link or fetch date, because it describes what Muxr
      is not and makes no claim about the other product. Nothing else about
      that product may be written.
- [ ] No hosting-vendor or platform names anywhere (text, comments, links).
- [ ] No "only", "official" or superlatives (best, fastest, ultimate) about Muxr.
- [ ] No tracking, no third-party script, no new external host.

## 2. Head

- [ ] `<title>` is 60 characters or fewer and ends ` · Muxr`.
- [ ] `<meta name="description">` is 155 characters or fewer.
- [ ] `<link rel="canonical" href="https://muxr.app/<path>">`. For a section
      index the canonical is the directory, `https://muxr.app/<section>/`.
- [ ] `<link rel="alternate" type="text/markdown" href="/<path>.md">`, for
      example `/how-to/zellij-on-android.html.md`. For a section index it is
      `/<section>/index.html.md`.
- [ ] Open Graph: `og:type` is `article`; `og:title`, `og:description`,
      `og:url` (equal to the canonical) and `og:image`
      `https://muxr.app/assets/img/og-card.png`. Keep `og:site_name`,
      the image size and alt tags and `twitter:card` from about.html.
- [ ] Exactly one `<script type="application/ld+json">` block, an `@graph`
      holding an `Article` (use `TechArticle` on how-to pages) and a
      `BreadcrumbList`. The Article has `headline`, `description`,
      `datePublished` and `dateModified` (both `2026-11-15`), `author` and
      `publisher` both `{"@id":"https://muxr.app/#organization"}`, and
      `mainEntityOfPage`. The headline and description equal the visible h1
      and meta description, and the breadcrumb list equals the visible
      breadcrumb. It must parse with `python3 -m json.tool`.
- [ ] Stylesheet is `/assets/css/glyph.css?v=4`; script is `/assets/js/main.js`.

## 3. Paths

New pages live in subdirectories, so a relative path copied from an existing
page breaks under `/how-to/` or `/compare/`.

- [ ] EVERY `href` and `src` is root-absolute: `/assets/css/glyph.css?v=4`,
      `/assets/js/main.js`, `/assets/img/logo.svg`, `/favicon.png`,
      `/index.html`, `/docs.html`, `/faq.html`, `/compare/` and so on.
- [ ] `grep -n 'href="assets\|src="assets\|href="[a-z]' page.html` finds no
      relative link (external `https://` and `mailto:` links are fine).

## 4. Body

- [ ] The shared `.site-nav` and `.site-footer`, copied from about.html.
- [ ] The active nav link is hard-coded per page with the `active` class:
      Compare on `/compare/`, `/alternatives/` and `/best/` pages; FAQ on
      `faq.html`; none elsewhere. about.html has no active link.
- [ ] A breadcrumb (`<nav class="breadcrumb">` with an `<ol>`), then exactly
      one `<h1>`.
- [ ] A visible line `<p class="updated">Last updated: 15 November 2026</p>`
      directly under the h1.
- [ ] All content sits inside `<main class="article">` so the markdown
      generator can find it. Nav and footer stay outside `<main>`.
- [ ] No inline style, no `style=` attribute, and no script other than the
      JSON-LD block and the external main.js. The CSP is
      `default-src 'self'`.
- [ ] Only classes that exist in `assets/css/glyph.css`. Page components:
      `.article`, `.article-lead`, `.breadcrumb`, `.updated`, `.faq-item`,
      `.verdict` (with `.verdict-title`), `.page-links` (with `.page-links-label`), `.hub-grid` (with
      `.hub-card`, `.hub-card-title`, `.hub-card-desc`), `.soon`, plus
      `.compare-wrap` and `.compare-table` for every comparison table and
      `.code-block` for commands. Need something new? Ask the conductor; do
      not add CSS from a page card.
- [ ] A table sits in `<div class="compare-wrap"><table class="compare-table">`
      with a `<caption>`, `<th scope="col">` headers and `<th scope="row">`
      row labels. Phone width must not scroll sideways: the wrapper scrolls
      the table, the page never does.

## 5. Writing

- [ ] The first two sentences answer the title's question.
- [ ] h2 headings are questions ("Does Muxr work with herdr?").
- [ ] Short paragraphs. Tables for comparisons. Plain, neutral tone.
- [ ] Say where the other product is the better pick (CLAIMS.md "Our
      judgement" lines give the wording).
- [ ] 400 to 900 words of body text (about.html is the one exception: 300 to 500).
- [ ] End with 2 to 4 links to related pages, inside
      `<nav class="page-links" aria-label="Related pages">` with no heading: a
      short `<p class="page-links-label">Related pages</p>` then a `<ul>`. This
      block is the one place a page has no question-form h2.
- [ ] A coming-soon item says "coming soon" and nothing about timing.

## 6. URL map

Fixed, so pages can link to siblings before they exist.

```
/about.html  /faq.html  /pricing.html  /security.html  /roadmap.html

/how-to/
  index.html
  zellij-on-iphone-and-ipad.html
  zellij-on-android.html
  herdr-on-your-phone.html
  claude-code-and-codex-on-your-phone.html

/compare/
  index.html
  moshi.html
  zelmux.html
  termius.html
  blink-shell.html
  zellij-web-client.html
  ssh-and-zellij.html
  herdr-mobile.html
  claude-code-remote-control.html

/alternatives/
  moshi.html
  termius.html

/best/
  zellij-mobile-clients.html
  herdr-mobile-clients.html
  apps-for-coding-agents-on-your-phone.html
```

Existing pages: `/index.html`, `/docs.html`, `/advanced.html`, `/guide.html`,
`/support.html`, `/privacy.html`, `/terms.html`. A directory link ends in a
slash (`/compare/`).

## 7. Checks before you hand off

1. Parse the JSON-LD: extract the block and run it through
   `python3 -m json.tool`.
2. Serve the site: `python3 -m http.server <port>` from `website/`, then
   `curl -s -o /dev/null -w '%{http_code}'` for the page and
   `/assets/css/glyph.css`; both must be 200.
3. `grep -n 'open.source\|style=\|href="assets\|src="assets' page.html` finds
   nothing about Muxr; `grep -c '9.99' page.html` shows the price sentence
   appears verbatim wherever a price is given.
4. Look at the page at 375 px and 1280 px wide: no horizontal page scroll.
5. Count the words in `<main>`: 400 to 900.
6. `git diff --stat` shows only your declared files.
