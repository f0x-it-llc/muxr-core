(function(){
  'use strict';

  // Signal that JS is active — glyph.css scopes hidden-until-revealed states
  // to `html.js` so content stays visible when JS fails/is disabled.
  document.documentElement.classList.add('js');

  // nav toggle (mobile)
  var toggle = document.getElementById('nav-toggle');
  var links = document.getElementById('nav-links');
  if(toggle && links){
    toggle.addEventListener('click', function(){
      var open = links.classList.toggle('open');
      toggle.setAttribute('aria-expanded', open ? 'true' : 'false');
    });
    links.querySelectorAll('a').forEach(function(a){
      a.addEventListener('click', function(){
        links.classList.remove('open');
        toggle.setAttribute('aria-expanded', 'false');
      });
    });
  }

  // copy-to-clipboard for doc code snippets
  function copyText(text){
    if(navigator.clipboard && navigator.clipboard.writeText){
      return navigator.clipboard.writeText(text);
    }
    // Fallback for non-secure contexts / older browsers
    return new Promise(function(resolve, reject){
      try{
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
      } catch(err){
        reject(err);
      }
    });
  }

  function resetCopyBtn(btn){
    btn.classList.remove('copied');
    btn.textContent = 'copy';
  }

  function showCopied(btn){
    btn.classList.add('copied');
    btn.textContent = 'copied';
    window.clearTimeout(btn._copyTimer);
    btn._copyTimer = window.setTimeout(function(){ resetCopyBtn(btn); }, 1500);
  }

  var codeBlocks = document.querySelectorAll('.code-block');
  codeBlocks.forEach(function(block){
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'copy-btn';
    btn.setAttribute('aria-label', 'Copy code to clipboard');
    btn.textContent = 'copy';
    block.appendChild(btn);

    btn.addEventListener('click', function(){
      var source = block.querySelector('code') || block.querySelector('pre');
      var text = source ? source.innerText : '';
      copyText(text).then(function(){
        showCopied(btn);
      }).catch(function(){
        btn.textContent = 'error';
        window.setTimeout(function(){ resetCopyBtn(btn); }, 1500);
      });
    });
  });

  // docs sidebar scrollspy
  var navLinks = Array.prototype.slice.call(document.querySelectorAll('.doc-nav-link'));
  if(navLinks.length){
    var sections = navLinks.map(function(l){ return document.querySelector(l.getAttribute('href')); }).filter(Boolean);
    var byId = {};
    navLinks.forEach(function(l){ byId[l.getAttribute('href').slice(1)] = l; });

    var observer = new IntersectionObserver(function(entries){
      entries.forEach(function(entry){
        if(entry.isIntersecting){
          navLinks.forEach(function(l){ l.classList.remove('active'); });
          var link = byId[entry.target.id];
          if(link) link.classList.add('active');
        }
      });
    }, { rootMargin: '-15% 0px -70% 0px', threshold: 0 });

    sections.forEach(function(s){ observer.observe(s); });
  }

  // reveal-on-scroll for feature cards
  var revealEls = document.querySelectorAll('.feature-card');
  if('IntersectionObserver' in window){
    if(revealEls.length){
      var ro = new IntersectionObserver(function(entries){
        entries.forEach(function(entry){
          if(entry.isIntersecting){
            var el = entry.target;
            var idx = Array.prototype.indexOf.call(revealEls, el);
            setTimeout(function(){ el.classList.add('in'); }, (idx % 4) * 80);
            ro.unobserve(el);
          }
        });
      }, { threshold: 0.2 });
      revealEls.forEach(function(el){ ro.observe(el); });
    }
  } else {
    revealEls.forEach(function(el){ el.classList.add('in'); });
  }

  // staggered line reveal for .terminal-body blocks (signature move, replayed on scroll into view)
  var termBlocks = document.querySelectorAll('.terminal-body');
  if('IntersectionObserver' in window){
    termBlocks.forEach(function(block){
      var lines = block.querySelectorAll('.term-line');
      if(!lines.length) return;
      var played = false;
      var io = new IntersectionObserver(function(entries){
        entries.forEach(function(entry){
          if(entry.isIntersecting && !played){
            played = true;
            lines.forEach(function(l, i){
              setTimeout(function(){ l.classList.add('show'); }, i * 90);
            });
            io.disconnect();
          }
        });
      }, { threshold: 0.3 });
      io.observe(block);
    });
  } else {
    termBlocks.forEach(function(block){
      block.querySelectorAll('.term-line').forEach(function(l){ l.classList.add('show'); });
    });
  }
})();
