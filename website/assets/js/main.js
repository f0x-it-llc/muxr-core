(function(){
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

  // reveal-on-scroll for feature cards / terminal blocks
  var revealEls = document.querySelectorAll('.feature-card');
  if(revealEls.length && 'IntersectionObserver' in window){
    var ro = new IntersectionObserver(function(entries){
      entries.forEach(function(entry, i){
        if(entry.isIntersecting){
          var el = entry.target;
          var idx = Array.prototype.indexOf.call(revealEls, el);
          setTimeout(function(){ el.classList.add('in'); }, (idx % 4) * 80);
          ro.unobserve(el);
        }
      });
    }, { threshold: 0.2 });
    revealEls.forEach(function(el){ ro.observe(el); });
  } else {
    revealEls.forEach(function(el){ el.classList.add('in'); });
  }

  // staggered line reveal for .terminal-body blocks (signature move, replayed on scroll into view)
  var termBlocks = document.querySelectorAll('.terminal-body');
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
})();
