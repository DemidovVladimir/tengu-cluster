/* Tengu field manual — shared behaviour for docs/tutorial/*.html.
   No libraries. Components (all opt-in by markup, see ../AUTHORING.md):
     rail + pager      <aside class="rail" id="rail"> · <nav class="pager" id="pager">
     theme toggle      built into the rail
     stage player      <figure class="stage" data-stage> svg [data-at] + ol.captions
     tokens            <circle class="tok" data-path="<path id>" data-at="3">
     tabs              <div class="tabs"> .tablist button[aria-controls] + .tabpanel
     terminal typer    <pre class="term" data-type> with one <span class="ln"> per line
     bars              <div class="fill" style="--w:83%">
     in-view hook      [data-inview] gets .in-view once visible */
(function () {
  "use strict";

  var reduce = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var T = window.TUTORIAL || { chapters: [], pages: [] };
  var here = (location.pathname.split("/").pop() || "index.html").replace(/\.html$/, "") || "index";

  function el(tag, attrs, html) {
    var e = document.createElement(tag);
    if (attrs) for (var k in attrs) e.setAttribute(k, attrs[k]);
    if (html != null) e.innerHTML = html;
    return e;
  }
  function store(key, val) {
    try {
      if (val === undefined) return localStorage.getItem(key);
      if (val === null) localStorage.removeItem(key); else localStorage.setItem(key, val);
    } catch (e) { return null; }
  }

  /* ---------- theme ---------- */
  var THEME_KEY = "tengu-tutorial-theme";
  function applyTheme(t) {
    if (t === "light" || t === "dark") document.documentElement.setAttribute("data-theme", t);
    else document.documentElement.removeAttribute("data-theme");
  }
  applyTheme(store(THEME_KEY));
  function themeButton() {
    var b = el("button", { type: "button", "class": "theme-btn" });
    function label() {
      var t = store(THEME_KEY) || "system";
      b.textContent = "Theme: " + t;
      b.setAttribute("aria-label", "Colour theme: " + t + ". Click to change.");
    }
    b.addEventListener("click", function () {
      var order = ["system", "light", "dark"];
      var cur = store(THEME_KEY) || "system";
      var next = order[(order.indexOf(cur) + 1) % order.length];
      store(THEME_KEY, next === "system" ? null : next);
      applyTheme(next);
      label();
    });
    label();
    return b;
  }

  /* ---------- rail + mobile bar ---------- */
  function buildRail() {
    var rail = document.getElementById("rail");
    if (!rail) return;
    rail.innerHTML = "";
    var brand = el("a", { "class": "brand", href: "index.html" },
      '<span class="brand-mark" aria-hidden="true">天</span><span><span class="brand-name">Tengu</span><span class="brand-sub">Field manual</span></span>');
    rail.appendChild(brand);
    var toc = el("nav", { "class": "toc", "aria-label": "Features" });
    T.chapters.forEach(function (c) {
      var pages = T.pages.filter(function (p) { return p.chapter === c.id; });
      if (!pages.length) return;
      var box = el("div", { "class": "toc-chapter" });
      box.appendChild(el("span", null, "<b>" + c.kanji + "</b>" + c.name));
      pages.forEach(function (p) {
        var a = el("a", { href: p.slug + ".html" }, p.title);
        if (p.slug === here) a.setAttribute("aria-current", "page");
        box.appendChild(a);
      });
      toc.appendChild(box);
    });
    rail.appendChild(toc);
    var foot = el("div", { "class": "rail-foot" });
    foot.appendChild(themeButton());
    rail.appendChild(foot);

    var bar = el("div", { "class": "mobile-bar" });
    bar.appendChild(el("a", { "class": "brand", href: "index.html" },
      '<span class="brand-mark" aria-hidden="true">天</span><span class="brand-name">Tengu</span>'));
    var menu = el("button", { type: "button", "class": "btn", "aria-expanded": "false", "aria-controls": "rail" }, "Features");
    menu.addEventListener("click", function () {
      var open = document.body.classList.toggle("rail-open");
      menu.setAttribute("aria-expanded", open ? "true" : "false");
    });
    bar.appendChild(menu);
    var main = document.querySelector("main");
    if (main) main.parentNode.insertBefore(bar, main);
    document.addEventListener("keydown", function (e) {
      if (e.key === "Escape" && document.body.classList.contains("rail-open")) {
        document.body.classList.remove("rail-open");
        menu.setAttribute("aria-expanded", "false");
      }
    });
    document.addEventListener("click", function (e) {
      if (!document.body.classList.contains("rail-open")) return;
      if (rail.contains(e.target) || menu.contains(e.target)) return;
      document.body.classList.remove("rail-open");
      menu.setAttribute("aria-expanded", "false");
    });
  }

  /* ---------- pager ---------- */
  function buildPager() {
    var pager = document.getElementById("pager");
    if (!pager) return;
    var i = T.pages.findIndex(function (p) { return p.slug === here; });
    var prev = i > 0 ? T.pages[i - 1] : (i === 0 ? { slug: "index", title: "All features" } : null);
    var next = i >= 0 && i < T.pages.length - 1 ? T.pages[i + 1] : null;
    if (here === "index") next = T.pages[0];
    pager.innerHTML = "";
    if (prev) pager.appendChild(el("a", { href: prev.slug + ".html", "class": "prev" }, "<small>Previous</small><span>" + prev.title + "</span>"));
    if (next) pager.appendChild(el("a", { href: next.slug + ".html", "class": "next" }, "<small>Next</small><span>" + next.title + "</span>"));
  }

  /* ---------- index cards ---------- */
  function buildIndex() {
    var host = document.getElementById("chapters");
    if (!host) return;
    host.innerHTML = "";
    T.chapters.forEach(function (c) {
      var pages = T.pages.filter(function (p) { return p.chapter === c.id; });
      if (!pages.length) return;
      var sec = el("section", { "class": "sec", id: "ch-" + c.id });
      sec.appendChild(el("div", { "class": "chapter-head" },
        '<span class="stamp" aria-hidden="true">' + c.kanji + "</span><h2>" + c.name + "</h2><p>" + c.blurb + "</p>"));
      var grid = el("div", { "class": "feature-grid" });
      pages.forEach(function (p) {
        grid.appendChild(el("a", { "class": "feature", href: p.slug + ".html" }, "<b>" + p.title + "</b><span>" + p.blurb + "</span>"));
      });
      sec.appendChild(grid);
      host.appendChild(sec);
    });
  }

  /* ---------- step ranges: "3" "1,3" "2-4" "3-" "*" ---------- */
  function inRange(spec, s) {
    if (!spec) return false;
    return spec.split(",").some(function (part) {
      part = part.trim();
      if (part === "*") return true;
      var m = part.match(/^(\d+)?-(\d+)?$/);
      if (m) {
        var lo = m[1] ? +m[1] : 1, hi = m[2] ? +m[2] : Infinity;
        return s >= lo && s <= hi;
      }
      return +part === s;
    });
  }

  /* ---------- stage player ---------- */
  function Stage(fig) {
    var svg = fig.querySelector("svg");
    var caps = Array.prototype.slice.call(fig.querySelectorAll(".captions > li"));
    var marked = Array.prototype.slice.call(fig.querySelectorAll("[data-at]"));
    var toks = Array.prototype.slice.call(fig.querySelectorAll(".tok[data-path]"));
    var n = caps.length || +fig.getAttribute("data-steps") || 1;
    var interval = +fig.getAttribute("data-interval") || 3400;
    var step = 1, timer = null, playing = false, touched = false, visible = false, raf = null, t0 = 0;

    var side = fig.querySelector(".stage-side");
    var controls = el("div", { "class": "stage-controls" });
    var bPrev = el("button", { type: "button", "class": "btn", "aria-label": "Previous step" }, "&#9664; Prev");
    var bPlay = el("button", { type: "button", "class": "btn", "aria-pressed": "false" }, "&#9654; Play");
    var bNext = el("button", { type: "button", "class": "btn", "aria-label": "Next step" }, "Next &#9654;");
    var count = el("span", { "class": "count", "aria-live": "polite" });
    controls.appendChild(bPrev); controls.appendChild(bPlay); controls.appendChild(bNext); controls.appendChild(count);
    if (side) side.insertBefore(controls, side.firstChild);

    function paths() {
      toks.forEach(function (t) {
        if (!t._path) t._path = svg && svg.getElementById ? svg.getElementById(t.getAttribute("data-path")) : document.getElementById(t.getAttribute("data-path"));
      });
    }
    function placeTok(t, frac) {
      var p = t._path;
      if (!p || !p.getTotalLength) return;
      var len = p.getTotalLength();
      var f = t.hasAttribute("data-reverse") ? 1 - frac : frac;
      var pt = p.getPointAtLength(len * Math.max(0, Math.min(1, f)));
      t.setAttribute("cx", pt.x); t.setAttribute("cy", pt.y);
    }
    function frame(now) {
      var any = false;
      toks.forEach(function (t) {
        if (!t.classList.contains("is-on")) { t.style.opacity = 0; return; }
        any = true;
        var dur = +t.getAttribute("data-dur") || 1500;
        var delay = +t.getAttribute("data-delay") || 0;
        var e = now - t0 - delay;
        if (e < 0) { t.style.opacity = 0; return; }
        var once = t.hasAttribute("data-once");
        var frac = once ? Math.min(1, e / dur) : (e % (dur + 350)) / dur;
        if (frac > 1) { t.style.opacity = 0; return; }
        t.style.opacity = 1;
        placeTok(t, frac);
      });
      if (any && visible) raf = requestAnimationFrame(frame); else raf = null;
    }
    function show(s) {
      step = ((s - 1 + n) % n) + 1;
      marked.forEach(function (m) {
        var on = inRange(m.getAttribute("data-at"), step);
        m.classList.toggle("is-on", on);
        m.classList.toggle("is-off", !on);
      });
      caps.forEach(function (c, i) {
        c.classList.toggle("is-on", i + 1 === step);
        c.classList.toggle("is-done", i + 1 < step);
        c.setAttribute("aria-current", i + 1 === step ? "step" : "false");
      });
      count.textContent = step + " / " + n;
      paths();
      if (reduce) {
        toks.forEach(function (t) {
          var on = t.classList.contains("is-on");
          t.style.opacity = on ? 1 : 0;
          if (on) placeTok(t, 0.55);
        });
      } else {
        t0 = performance.now();
        if (!raf) raf = requestAnimationFrame(frame);
      }
      markEdges(fig);
      fig.dispatchEvent(new CustomEvent("stage:step", { detail: { step: step, steps: n } }));
    }
    function tick() { show(step + 1); }
    function play() {
      if (playing) return;
      playing = true;
      bPlay.innerHTML = "&#10074;&#10074; Pause"; bPlay.setAttribute("aria-pressed", "true");
      timer = setInterval(tick, interval);
    }
    function pause() {
      playing = false;
      bPlay.innerHTML = "&#9654; Play"; bPlay.setAttribute("aria-pressed", "false");
      clearInterval(timer); timer = null;
    }
    function user(fn) { return function () { touched = true; pause(); fn(); }; }
    bPrev.addEventListener("click", user(function () { show(step - 1); }));
    bNext.addEventListener("click", user(function () { show(step + 1); }));
    bPlay.addEventListener("click", function () { touched = true; if (playing) pause(); else { if (step === n) show(1); play(); } });
    caps.forEach(function (c, i) {
      c.setAttribute("tabindex", "0");
      c.setAttribute("role", "button");
      var go = user(function () { show(i + 1); });
      c.addEventListener("click", go);
      c.addEventListener("keydown", function (e) { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); go(); } });
    });
    if ("IntersectionObserver" in window) {
      new IntersectionObserver(function (es) {
        es.forEach(function (e) {
          visible = e.isIntersecting;
          if (visible) {
            if (!raf && !reduce) { t0 = performance.now(); raf = requestAnimationFrame(frame); }
            if (!touched && !reduce) play();
          } else if (playing) {
            pause();
          }
        });
      }, { threshold: 0.35 }).observe(fig);
    } else {
      visible = true;
    }
    show(1);
    fig._stage = { show: show, play: play, pause: pause };
  }

  /* ---------- tabs ---------- */
  function Tabs(box) {
    var btns = Array.prototype.slice.call(box.querySelectorAll(".tablist button"));
    function sel(b) {
      btns.forEach(function (x) {
        var on = x === b;
        x.setAttribute("aria-selected", on ? "true" : "false");
        x.setAttribute("tabindex", on ? "0" : "-1");
        var p = document.getElementById(x.getAttribute("aria-controls"));
        if (p) p.hidden = !on;
      });
      box.dispatchEvent(new CustomEvent("tabs:select", { detail: { id: b.getAttribute("aria-controls") } }));
    }
    btns.forEach(function (b, i) {
      b.setAttribute("role", "tab");
      b.addEventListener("click", function () { sel(b); });
      b.addEventListener("keydown", function (e) {
        var d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
        if (!d) return;
        var nb = btns[(i + d + btns.length) % btns.length];
        nb.focus(); sel(nb);
      });
    });
    var tl = box.querySelector(".tablist");
    if (tl) tl.setAttribute("role", "tablist");
    var first = btns.filter(function (b) { return b.getAttribute("aria-selected") === "true"; })[0] || btns[0];
    if (first) sel(first);
  }

  /* ---------- terminal typer ---------- */
  function Term(pre) {
    var lines = Array.prototype.slice.call(pre.querySelectorAll(".ln"));
    if (!lines.length) return;
    var wrap = pre.parentNode && pre.parentNode.classList.contains("term-wrap") ? pre.parentNode : null;
    var running = false;
    function run() {
      if (reduce || running) return;
      running = true;
      lines.forEach(function (l) { l.style.visibility = "hidden"; });
      var i = 0;
      (function next() {
        if (i >= lines.length) { running = false; return; }
        var l = lines[i++];
        l.style.visibility = "visible";
        if (l.classList.contains("cmd")) {
          var len = l.textContent.length;
          var dur = Math.min(1400, 18 * len + 120);
          l.animate([{ clipPath: "inset(0 100% 0 0)" }, { clipPath: "inset(0 0 0 0)" }], { duration: dur, easing: "steps(" + Math.max(4, len) + ")" });
          setTimeout(next, dur + 220);
        } else {
          setTimeout(next, 140);
        }
      })();
    }
    if (wrap) {
      var b = el("button", { type: "button", "class": "btn" }, "&#8635; Replay");
      b.addEventListener("click", run);
      wrap.appendChild(b);
    }
    lines.forEach(function (l) { l.style.display = "block"; });
    onView(pre, run);
  }

  /* ---------- in-view helper ---------- */
  function onView(node, fn) {
    if (!("IntersectionObserver" in window)) { fn(); return; }
    var io = new IntersectionObserver(function (es) {
      es.forEach(function (e) { if (e.isIntersecting) { io.disconnect(); fn(); } });
    }, { threshold: 0.3 });
    io.observe(node);
  }
  window.TenguTutorial = { onView: onView, inRange: inRange, reduce: reduce };

  /* ---------- bars ---------- */
  function Bars(box) {
    if (reduce) return;
    onView(box, function () { box.classList.add("in-view"); });
  }

  /* ---------- shared SVG defs: arrowheads for .e edges ----------
     Built with createElementNS, and set as a `marker-end` attribute on each
     edge (a url(#id) inside site.css would resolve against the stylesheet). */
  var NS = "http://www.w3.org/2000/svg";
  function injectDefs() {
    if (document.getElementById("arrow")) return;
    var s = document.createElementNS(NS, "svg");
    s.setAttribute("width", "0"); s.setAttribute("height", "0");
    s.setAttribute("aria-hidden", "true"); s.setAttribute("focusable", "false");
    s.style.position = "absolute"; s.style.width = "0"; s.style.height = "0"; s.style.overflow = "hidden";
    var d = document.createElementNS(NS, "defs");
    [["arrow", "var(--faint)"], ["arrow-on", "var(--accent)"], ["arrow-bad", "var(--bad)"]].forEach(function (m) {
      var mk = document.createElementNS(NS, "marker");
      mk.setAttribute("id", m[0]);
      mk.setAttribute("viewBox", "0 0 10 10");
      mk.setAttribute("refX", "9"); mk.setAttribute("refY", "5");
      mk.setAttribute("markerWidth", "7"); mk.setAttribute("markerHeight", "7");
      mk.setAttribute("orient", "auto-start-reverse");
      var p = document.createElementNS(NS, "path");
      p.setAttribute("d", "M0 0 L10 5 L0 10 z");
      p.style.fill = m[1];
      mk.appendChild(p);
      d.appendChild(mk);
    });
    s.appendChild(d);
    document.body.insertBefore(s, document.body.firstChild);
  }
  function markEdges(root) {
    Array.prototype.forEach.call((root || document).querySelectorAll("svg .e"), function (e) {
      if (e.hasAttribute("data-no-arrow")) return;
      var id = e.classList.contains("bad") ? "arrow-bad" : e.classList.contains("is-on") ? "arrow-on" : "arrow";
      e.setAttribute("marker-end", "url(#" + id + ")");
    });
  }
  window.TenguTutorial.markEdges = markEdges;

  function init() {
    injectDefs();
    buildRail();
    buildPager();
    buildIndex();
    Array.prototype.forEach.call(document.querySelectorAll("figure.stage"), Stage);
    markEdges(document);
    Array.prototype.forEach.call(document.querySelectorAll(".tabs"), Tabs);
    Array.prototype.forEach.call(document.querySelectorAll("pre.term[data-type]"), Term);
    Array.prototype.forEach.call(document.querySelectorAll(".bars"), Bars);
    Array.prototype.forEach.call(document.querySelectorAll("[data-inview]"), function (n) {
      onView(n, function () { n.classList.add("in-view"); });
    });
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init); else init();
})();
