/* ═══════════════════════════════════════════════
   Muxr website — vanilla JS, no dependencies.
   - Mobile nav toggle (hamburger)
   - Copy-to-clipboard buttons for .code-block
   - Scroll-spy for .doc-sidebar (no-ops on pages without one)
   Must throw no console errors on any page.
═══════════════════════════════════════════════ */
(function () {
  'use strict';

  /* ── Mobile nav toggle ─────────────────────── */
  function initNavToggle() {
    var toggle = document.querySelector('.nav-toggle');
    var links = document.getElementById('nav-links');
    if (!toggle || !links) return;

    toggle.addEventListener('click', function () {
      var open = links.classList.toggle('open');
      toggle.setAttribute('aria-expanded', open ? 'true' : 'false');
    });

    // Close the menu when a link is chosen (mobile UX)
    links.addEventListener('click', function (e) {
      if (e.target.closest('a')) {
        links.classList.remove('open');
        toggle.setAttribute('aria-expanded', 'false');
      }
    });
  }

  /* ── Copy-to-clipboard for code blocks ─────── */
  function copyText(text) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      return navigator.clipboard.writeText(text);
    }
    // Fallback for non-secure contexts / older browsers
    return new Promise(function (resolve, reject) {
      try {
        var ta = document.createElement('textarea');
        ta.value = text;
        ta.setAttribute('readonly', '');
        ta.style.position = 'absolute';
        ta.style.left = '-9999px';
        document.body.appendChild(ta);
        ta.select();
        var ok = document.execCommand('copy');
        document.body.removeChild(ta);
        ok ? resolve() : reject(new Error('execCommand copy failed'));
      } catch (err) {
        reject(err);
      }
    });
  }

  function initCopyButtons() {
    var blocks = document.querySelectorAll('.code-block');
    Array.prototype.forEach.call(blocks, function (block) {
      // Skip if author already provided a copy button.
      var btn = block.querySelector('.copy-btn');
      if (!btn) {
        btn = document.createElement('button');
        btn.type = 'button';
        btn.className = 'copy-btn';
        btn.setAttribute('aria-label', 'Copy code');
        btn.textContent = 'copy';
        block.appendChild(btn);
      }

      btn.addEventListener('click', function () {
        var source = block.querySelector('code') || block.querySelector('pre');
        var text = source ? source.innerText : '';
        copyText(text).then(function () {
          showCopied(btn);
        }).catch(function () {
          btn.textContent = 'error';
          window.setTimeout(function () { resetBtn(btn); }, 1500);
        });
      });
    });
  }

  function showCopied(btn) {
    btn.classList.add('copied');
    btn.textContent = 'copied';
    window.clearTimeout(btn._copyTimer);
    btn._copyTimer = window.setTimeout(function () { resetBtn(btn); }, 1500);
  }

  function resetBtn(btn) {
    btn.classList.remove('copied');
    btn.textContent = 'copy';
  }

  /* ── Scroll-spy (doc pages only) ───────────── */
  function initScrollSpy() {
    var sidebar = document.querySelector('.doc-sidebar');
    if (!sidebar) return; // no-op on non-doc pages

    var links = Array.prototype.slice.call(sidebar.querySelectorAll('.doc-nav-link[href^="#"]'));
    if (!links.length) return;

    var byId = {};
    var sections = [];
    links.forEach(function (link) {
      var id = link.getAttribute('href').slice(1);
      var section = id && document.getElementById(id);
      if (section) {
        byId[id] = link;
        sections.push(section);
      }
    });
    if (!sections.length) return;

    if (!('IntersectionObserver' in window)) return; // graceful no-op

    var visible = {};
    function setActive(id) {
      links.forEach(function (l) { l.classList.remove('active'); });
      if (byId[id]) byId[id].classList.add('active');
    }

    var observer = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        visible[entry.target.id] = entry.isIntersecting;
      });
      // Pick the first section currently in view, in document order.
      for (var i = 0; i < sections.length; i++) {
        if (visible[sections[i].id]) {
          setActive(sections[i].id);
          return;
        }
      }
    }, {
      rootMargin: '-20% 0px -70% 0px',
      threshold: 0
    });

    sections.forEach(function (s) { observer.observe(s); });
    // Highlight the first link until the observer fires.
    setActive(sections[0].id);
  }

  /* ── Boot ──────────────────────────────────── */
  function init() {
    initNavToggle();
    initCopyButtons();
    initScrollSpy();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
