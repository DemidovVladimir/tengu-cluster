// Tengu Studio Builder — a drag-and-drop canvas over one sandbox's
// blueprint (`sandboxes/<name>/builder.json`). Everything that means
// something arrives from Rust (/api/v1/builder…): the palette (kinds, their
// fields, the tools with their own descriptions and JSON schemas, the
// connections, the engines, the secret backends), every card's title and
// subtitle, every edge's state (live = green electricity · warn · error),
// every issue, the generated TOML, its diff, the real config-loader verdict
// and the secrets checklist. Here: pixels, pointer handling, a form per
// FieldSpec, undo/redo of blueprint snapshots, autosave (PUT) and the two
// change requests (preview, finalise). `palette.connections` is used only
// to hint drop targets while dragging an edge; Rust decides whether an edge
// is valid. Identifiers are always shown whole — wrapped, never shortened.
"use strict";

(() => {
  const CARD_W = 236; // card width (also in builder.css: .card)
  const PORT_Y = 27; // port centre from the card's top edge
  const SAVE_MS = 400; // field-edit debounce before the autosave PUT
  const VIEW_SAVE_MS = 900; // pan / zoom debounce
  const ZOOM_MIN = 0.25;
  const ZOOM_MAX = 2;
  const UNDO_MAX = 100;
  const LOG_MAX = 100;
  const DRAG_PX = 4;

  const $ = (id) => document.getElementById(id);
  const SVG_NS = "http://www.w3.org/2000/svg";
  const REDUCED = window.matchMedia("(prefers-reduced-motion: reduce)");

  const S = {
    token: null,
    tokenInHash: false,
    sandbox: "",
    editable: false,
    whyNot: null,
    palette: null,
    kinds: new Map(), // kind → kind spec
    bp: null, // the blueprint (sent as is: only contract fields)
    status: null,
    edgeState: new Map(), // edge id → last drawn state
    nodeEls: new Map(), // node id → { card, title, sub, badge, inPort, outPort }
    edgeEls: new Map(), // edge id → { g, line, glow, flow, hit, parts, label }
    sel: null, // { type: "node" | "edge", id }
    undo: [],
    redo: [],
    saving: false,
    dirty: false,
    saveTimer: null,
    viewTimer: null,
    saveSeq: 0,
    lastResponse: null,
    log: [],
    debugTab: "issues",
    preview: null,
    modalTab: "toml",
    busy: false,
    fieldIssueEls: new Map(), // field key → issues container (inspector)
    firstStatus: true,
  };

  // ---- small helpers -------------------------------------------------------

  function el(tag, text, cls) {
    const e = document.createElement(tag);
    if (text !== undefined && text !== null) e.textContent = String(text);
    if (cls) e.className = cls;
    return e;
  }

  function sv(tag, attrs) {
    const e = document.createElementNS(SVG_NS, tag);
    for (const [k, v] of Object.entries(attrs || {})) e.setAttribute(k, String(v));
    return e;
  }

  function clear(e) {
    while (e.firstChild) e.removeChild(e.firstChild);
    return e;
  }

  function rnd(n) {
    const a = new Uint8Array(n);
    crypto.getRandomValues(a);
    return Array.from(a, (b) => (b % 36).toString(36)).join("");
  }

  function clone(v) {
    return JSON.parse(JSON.stringify(v));
  }

  function typing(e) {
    const t = e.target;
    return t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT" || t.isContentEditable);
  }

  async function copyText(text, btn) {
    try {
      await navigator.clipboard.writeText(text);
    } catch (_) {
      const ta = el("textarea");
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      try { document.execCommand("copy"); } catch (_) { /* nothing left to try */ }
      ta.remove();
    }
    if (btn) {
      const was = btn.textContent;
      btn.textContent = "Copied ✓";
      btn.classList.add("copied");
      setTimeout(() => {
        btn.textContent = was;
        btn.classList.remove("copied");
      }, 1300);
    } else toast("Copied", "ok", { ms: 1400 });
  }

  function copyBtn(text, label) {
    const b = el("button", label || "Copy", "copy-btn");
    b.type = "button";
    b.addEventListener("click", (e) => {
      e.stopPropagation();
      copyText(text, b);
    });
    return b;
  }

  // ---- token (URL fragment, as studio.js) ----------------------------------

  function parseHash() {
    const out = {};
    for (const part of location.hash.replace(/^#/, "").split("&")) {
      const i = part.indexOf("=");
      if (i > 0) out[part.slice(0, i)] = decodeURIComponent(part.slice(i + 1));
    }
    return out;
  }

  // The token rides in the fragment (`#t=<hex>`), which a browser never
  // sends; kept for this tab (shared with the Studio page), then dropped
  // from the address bar.
  function readToken(h) {
    const given = /^[0-9a-f]{64}$/.test(h.t || "") ? h.t : null;
    try {
      if (given) sessionStorage.setItem("studio-token", given);
      return given || sessionStorage.getItem("studio-token");
    } catch (_) {
      S.tokenInHash = Boolean(given);
      return given;
    }
  }

  function writeHash() {
    const h = S.tokenInHash ? `#t=${encodeURIComponent(S.token)}` : "";
    if (location.hash !== h) history.replaceState(null, "", h || location.pathname);
  }

  // ---- API (every request logged for the debug drawer) ---------------------

  async function request(method, path, body) {
    const t0 = performance.now();
    const headers = { "X-Studio-Token": S.token };
    const init = { method, headers, cache: "no-store" };
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    let status = 0;
    let out = {};
    try {
      const r = await fetch(path, init);
      status = r.status;
      out = await r.json().catch(() => ({}));
    } catch (e) {
      out = { error: `network: ${e.message}` };
    }
    const ms = Math.round(performance.now() - t0);
    const entry = { at: new Date().toISOString(), method, path, status, ms };
    S.log.unshift(entry);
    if (S.log.length > LOG_MAX) S.log.length = LOG_MAX;
    S.lastResponse = { method, path, status, ms, body: out };
    console.debug("[builder]", method, path, status, `${ms}ms`, { sent: body, answer: out });
    if (!$("debug").hidden) drawDebug();
    return { status, body: out };
  }

  function errText(r) {
    return (r.body && r.body.error) || `HTTP ${r.status}`;
  }

  // ---- palette lookups -----------------------------------------------------

  function kindOf(k) {
    return S.kinds.get(k) || { kind: k, label: k, fields: [], icon: "", color: "slate", singleton: false, deletable: true };
  }

  function nodeById(id) {
    return S.bp.nodes.find((n) => n.id === id) || null;
  }

  function edgeById(id) {
    return S.bp.edges.find((e) => e.id === id) || null;
  }

  function connectionFor(fromKind, toKind) {
    return (S.palette.connections || []).find((c) => c.from === fromKind && c.to === toKind) || null;
  }

  function targetsOf(fromKind) {
    return (S.palette.connections || []).filter((c) => c.from === fromKind);
  }

  function hasOut(kind) {
    return (S.palette.connections || []).some((c) => c.from === kind);
  }

  function hasIn(kind) {
    return (S.palette.connections || []).some((c) => c.to === kind);
  }

  function itemFor(node) {
    // the palette item a node came from (tool / skill items carry a preset)
    const items = S.palette.items || [];
    return (
      items.find((i) => i.kind === node.kind && i.preset && Object.keys(i.preset).length &&
        Object.entries(i.preset).every(([k, v]) => !["string", "number"].includes(typeof v) || node.fields[k] === v) &&
        Object.keys(i.preset).some((k) => k === "tool" || k === "skill")) ||
      null
    );
  }

  function cardTitle(node) {
    const st = S.status && S.status.nodes && S.status.nodes[node.id];
    if (st && st.title) return st.title;
    const k = kindOf(node.kind);
    const v = k.title_field ? node.fields[k.title_field] : null;
    return v ? String(v) : `(new ${k.label.toLowerCase()})`;
  }

  function cardSub(node) {
    const st = S.status && S.status.nodes && S.status.nodes[node.id];
    if (st && st.subtitle) return st.subtitle;
    const k = kindOf(node.kind);
    return (k.subtitle_fields || [])
      .map((f) => node.fields[f])
      .filter((v) => v !== undefined && v !== null && v !== "" && !(Array.isArray(v) && !v.length))
      .map((v) => (Array.isArray(v) ? v.join(", ") : String(v)))
      .join(" · ");
  }

  // ---- icons (drawn here; names come from the palette) ---------------------

  const GLYPHS = {
    agent: ["M8 4h8M12 4v3", "M5 9a3 3 0 0 1 3-3h8a3 3 0 0 1 3 3v6a3 3 0 0 1-3 3H8a3 3 0 0 1-3-3z", "M9.5 12h.01M14.5 12h.01M10 15.5h4", "M3 11v3M21 11v3"],
    planner: ["M12 3v5", "M12 8 6 13M12 8l6 5", "M6 13v4M18 13v4", "M10 3h4", "M4.5 17h3v3h-3zM16.5 17h3v3h-3z"],
    tool: ["M14.7 6.3a4 4 0 0 0-5.4 5.4L4 17l3 3 5.3-5.3a4 4 0 0 0 5.4-5.4l-2.6 2.6-2.4-.6-.6-2.4z"],
    skill: ["M4 5.5A2.5 2.5 0 0 1 6.5 3H20v15H6.5A2.5 2.5 0 0 0 4 20.5z", "M4 20.5A2.5 2.5 0 0 0 6.5 23H20", "M12 7l1.2 2.5L16 10l-2 2 .5 2.8L12 13.5 9.5 14.8 10 12l-2-2 2.8-.5z"],
    workspace: ["M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z", "M3 10h18"],
    secret: ["M14 10a4 4 0 1 1-1.2-2.8", "M13.5 9.5 21 17v3h-3v-2h-2v-2h-2l-1.6-1.6", "M8.5 10h.01"],
    proxy: ["M7 18a4 4 0 0 1-.6-8 6 6 0 0 1 11.6 1.5A3.5 3.5 0 0 1 17.5 18z", "M12 11v4M10.5 13.5 12 15l1.5-1.5"],
    telegram: ["M21 4 3 11l6 2 2 6 3.5-4.5L19 18z", "M9 13l12-9"],
    webhook: ["M13 3 6 13h5l-1 8 8-11h-5z"],
    sandbox: ["M12 3 20 7.5v9L12 21l-8-4.5v-9z", "M12 12l8-4.5M12 12v9M12 12 4 7.5"],
    mcp: ["M9 3v5M15 3v5", "M7 8h10v3a5 5 0 0 1-10 0z", "M12 16v5"],
    link: ["M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1", "M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"],
    generic: ["M12 4a8 8 0 1 0 0 16 8 8 0 0 0 0-16z", "M12 9v6M9 12h6"],
  };

  function glyph(name, cls) {
    const s = sv("svg", { viewBox: "0 0 24 24", class: `glyph ${cls || ""}`, "aria-hidden": "true" });
    for (const d of GLYPHS[name] || GLYPHS.generic) {
      s.appendChild(sv("path", { d, fill: "none", stroke: "currentColor", "stroke-width": "1.7", "stroke-linecap": "round", "stroke-linejoin": "round" }));
    }
    return s;
  }

  function colorClass(c) {
    return `c-${/^[a-z]+$/.test(c || "") ? c : "slate"}`;
  }

  // ---- toasts + banner -----------------------------------------------------

  function toast(msg, tone, opts) {
    const o = opts || {};
    const t = el("div", null, `toast t-${tone || "info"}`);
    t.appendChild(el("div", msg, "toast-msg"));
    if (o.lines && o.lines.length) {
      const ul = el("ul", null, "toast-lines");
      for (const line of o.lines) {
        const li = el("li");
        li.appendChild(el("code", line));
        li.appendChild(copyBtn(line));
        ul.appendChild(li);
      }
      t.appendChild(ul);
    }
    const x = el("button", "×", "toast-x");
    x.type = "button";
    x.addEventListener("click", () => dismiss());
    t.appendChild(x);
    $("toasts").appendChild(t);
    requestAnimationFrame(() => t.classList.add("in"));
    const dismiss = () => {
      t.classList.remove("in");
      t.classList.add("out");
      setTimeout(() => t.remove(), 260);
    };
    if (o.ms !== 0) setTimeout(dismiss, o.ms || (tone === "error" ? 7000 : 4200));
    return t;
  }

  function banner(text, tone) {
    const b = $("banner");
    if (!text) {
      b.hidden = true;
      return;
    }
    b.textContent = text;
    b.className = `banner b-${tone || "info"}`;
    b.hidden = false;
  }

  // ---- view (pan / zoom) ---------------------------------------------------

  function view() {
    if (!S.bp.view) S.bp.view = { x: 0, y: 0, zoom: 1 };
    return S.bp.view;
  }

  function applyView() {
    const v = view();
    $("world").style.transform = `translate(${v.x}px, ${v.y}px) scale(${v.zoom})`;
    $("z-level").textContent = `${Math.round(v.zoom * 100)}%`;
    $("stage").style.backgroundPosition = `${v.x}px ${v.y}px`;
    $("stage").style.backgroundSize = `${22 * v.zoom}px ${22 * v.zoom}px`;
    drawMinimap();
  }

  function toWorld(clientX, clientY) {
    const r = $("stage").getBoundingClientRect();
    const v = view();
    return { x: (clientX - r.left - v.x) / v.zoom, y: (clientY - r.top - v.y) / v.zoom };
  }

  function zoomAt(clientX, clientY, factor) {
    const v = view();
    const r = $("stage").getBoundingClientRect();
    const z = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, v.zoom * factor));
    const px = clientX - r.left;
    const py = clientY - r.top;
    v.x = Math.round(px - ((px - v.x) * z) / v.zoom);
    v.y = Math.round(py - ((py - v.y) * z) / v.zoom);
    v.zoom = Math.round(z * 1000) / 1000;
    applyView();
    saveViewSoon();
  }

  function bounds() {
    if (!S.bp.nodes.length) return null;
    let x0 = Infinity;
    let y0 = Infinity;
    let x1 = -Infinity;
    let y1 = -Infinity;
    for (const n of S.bp.nodes) {
      const h = cardHeight(n.id);
      x0 = Math.min(x0, n.x);
      y0 = Math.min(y0, n.y);
      x1 = Math.max(x1, n.x + CARD_W);
      y1 = Math.max(y1, n.y + h);
    }
    return { x0, y0, x1, y1 };
  }

  function fit() {
    const b = bounds();
    const r = $("stage").getBoundingClientRect();
    const v = view();
    if (!b) {
      v.x = Math.round(r.width / 2 - CARD_W / 2);
      v.y = Math.round(r.height / 3);
      v.zoom = 1;
    } else {
      const pad = 70;
      const z = Math.min(1.15, Math.max(ZOOM_MIN, Math.min((r.width - pad * 2) / (b.x1 - b.x0), (r.height - pad * 2) / (b.y1 - b.y0))));
      v.zoom = Math.round(z * 1000) / 1000;
      v.x = Math.round((r.width - (b.x1 - b.x0) * z) / 2 - b.x0 * z);
      v.y = Math.round((r.height - (b.y1 - b.y0) * z) / 2 - b.y0 * z);
    }
    applyView();
    saveViewSoon();
  }

  function saveViewSoon() {
    clearTimeout(S.viewTimer);
    S.viewTimer = setTimeout(() => save(), VIEW_SAVE_MS);
  }

  // ---- undo / redo ---------------------------------------------------------

  function snapshot() {
    S.undo.push(JSON.stringify({ nodes: S.bp.nodes, edges: S.bp.edges }));
    if (S.undo.length > UNDO_MAX) S.undo.shift();
    S.redo.length = 0;
    drawUndo();
  }

  function restore(from, to) {
    if (!from.length || !S.editable) return;
    to.push(JSON.stringify({ nodes: S.bp.nodes, edges: S.bp.edges }));
    const snap = JSON.parse(from.pop());
    S.bp.nodes = snap.nodes;
    S.bp.edges = snap.edges;
    if (S.sel && !(S.sel.type === "node" ? nodeById(S.sel.id) : edgeById(S.sel.id))) S.sel = null;
    drawAll();
    drawInspector();
    drawUndo();
    save();
  }

  function drawUndo() {
    $("btn-undo").disabled = !S.undo.length || !S.editable;
    $("btn-redo").disabled = !S.redo.length || !S.editable;
  }

  // ---- autosave (PUT blueprint → Status) -----------------------------------

  function saveSoon(ms) {
    S.dirty = true;
    setSaveInd("pending");
    clearTimeout(S.saveTimer);
    S.saveTimer = setTimeout(() => save(), ms === undefined ? SAVE_MS : ms);
  }

  async function save() {
    clearTimeout(S.saveTimer);
    if (!S.editable) return;
    if (S.saving) {
      S.dirty = true;
      return;
    }
    S.saving = true;
    S.dirty = false;
    const seq = ++S.saveSeq;
    setSaveInd("saving");
    const r = await request("PUT", "/api/v1/builder/blueprint", S.bp);
    S.saving = false;
    if (r.status === 200 && r.body && r.body.nodes) {
      if (seq === S.saveSeq || !S.dirty) applyStatus(r.body);
      setSaveInd(S.dirty ? "pending" : "saved");
    } else {
      setSaveInd("error", errText(r));
      toast(`Save failed: ${errText(r)}`, "error");
    }
    if (S.dirty) save();
  }

  function setSaveInd(state, why) {
    const s = $("save-ind");
    s.className = `save-ind s-${state}`;
    s.textContent = {
      pending: "editing…",
      saving: "saving…",
      saved: "saved · checked",
      error: "not saved",
      readonly: "read-only",
    }[state] || state;
    s.title = why || "";
  }

  // ---- status from Rust → cards, edges, pill -------------------------------

  function applyStatus(st) {
    const prev = S.edgeState;
    S.status = st;
    for (const n of S.bp.nodes) drawCardState(n);
    const sparks = [];
    for (const e of S.bp.edges) {
      const es = (st.edges && st.edges[e.id]) || null;
      const state = es ? es.state : "pending";
      const before = prev.get(e.id);
      if (state === "live" && before !== "live") sparks.push(e.id);
      prev.set(e.id, state);
      drawEdgeState(e, es);
    }
    drawPill();
    if (S.sel) refreshInspectorIssues();
    else drawInspector();
    if (!$("debug").hidden) drawDebug();
    drawMinimap();
    sparks.forEach((id, i) => setTimeout(() => spark(id), S.firstStatus ? 120 * i : 0));
    S.firstStatus = false;
  }

  function drawPill() {
    const st = S.status;
    const p = $("status-pill");
    const fin = $("btn-finalise");
    if (!st) {
      p.className = "pill p-pending";
      p.textContent = "checking…";
      return;
    }
    const c = st.counts || { errors: 0, warnings: 0 };
    if (c.errors > 0) {
      p.className = "pill p-error";
      p.textContent = `${c.errors} error${c.errors === 1 ? "" : "s"}${c.warnings ? ` · ${c.warnings} warning${c.warnings === 1 ? "" : "s"}` : ""}`;
    } else if (c.warnings > 0) {
      p.className = "pill p-warn";
      p.textContent = `${c.warnings} warning${c.warnings === 1 ? "" : "s"}`;
    } else {
      p.className = "pill p-ok";
      p.textContent = "Ready";
    }
    p.title = "Click for the issues";
    fin.classList.toggle("ready", Boolean(st.ok) && S.editable);
  }

  // ---- cards ---------------------------------------------------------------

  function cardHeight(id) {
    const ne = S.nodeEls.get(id);
    return ne ? ne.card.offsetHeight || 64 : 64;
  }

  function makeCard(node, pop) {
    const k = kindOf(node.kind);
    const card = el("div", null, `card ${colorClass(k.color)} st-pending`);
    card.dataset.id = node.id;
    card.tabIndex = 0;
    const head = el("div", null, "card-head");
    const ic = el("span", null, "card-icon");
    ic.appendChild(glyph(k.icon));
    head.appendChild(ic);
    const tt = el("div", null, "card-titles");
    const kindLbl = el("span", k.label, "card-kind");
    const title = el("span", "", "card-title");
    tt.appendChild(kindLbl);
    tt.appendChild(title);
    head.appendChild(tt);
    const badge = el("span", "", "card-badge");
    badge.hidden = true;
    head.appendChild(badge);
    card.appendChild(head);
    const sub = el("div", "", "card-sub");
    card.appendChild(sub);
    let inPort = null;
    let outPort = null;
    if (hasIn(node.kind)) {
      inPort = el("span", null, "port port-in");
      inPort.title = "in";
      card.appendChild(inPort);
    }
    if (hasOut(node.kind)) {
      outPort = el("span", null, "port port-out");
      outPort.title = "Drag to connect";
      outPort.addEventListener("pointerdown", (e) => startConnect(e, node.id));
      card.appendChild(outPort);
    }
    card.addEventListener("pointerdown", (e) => startCardDrag(e, node.id));
    card.addEventListener("keydown", (e) => {
      if (e.key === "Enter") select({ type: "node", id: node.id });
    });
    $("cards").appendChild(card);
    const ne = { card, title, sub, badge, inPort, outPort };
    S.nodeEls.set(node.id, ne);
    placeCard(node);
    drawCardState(node);
    if (pop && !REDUCED.matches) {
      card.classList.add("pop");
      setTimeout(() => card.classList.remove("pop"), 520);
    }
    return ne;
  }

  function placeCard(node) {
    const ne = S.nodeEls.get(node.id);
    if (!ne) return;
    ne.card.style.left = `${node.x}px`;
    ne.card.style.top = `${node.y}px`;
  }

  function drawCardState(node) {
    const ne = S.nodeEls.get(node.id);
    if (!ne) return;
    const st = S.status && S.status.nodes && S.status.nodes[node.id];
    const state = st ? st.state : "pending";
    ne.card.classList.remove("st-ok", "st-warn", "st-error", "st-pending");
    ne.card.classList.add(`st-${state}`);
    ne.title.textContent = cardTitle(node);
    const sub = cardSub(node);
    ne.sub.textContent = sub;
    ne.sub.hidden = !sub;
    const issues = (st && st.issues) || [];
    const errs = issues.filter((i) => i.level === "error").length;
    ne.badge.hidden = !issues.length;
    ne.badge.textContent = String(issues.length);
    ne.badge.className = `card-badge ${errs ? "b-error" : "b-warn"}`;
    ne.badge.title = issues.map((i) => `${i.level}: ${i.field ? `${i.field}: ` : ""}${i.message}`).join("\n");
    ne.card.classList.toggle("sel", Boolean(S.sel && S.sel.type === "node" && S.sel.id === node.id));
  }

  // ---- edges ---------------------------------------------------------------

  function portPos(nodeId, side) {
    const n = nodeById(nodeId);
    if (!n) return { x: 0, y: 0 };
    return side === "out" ? { x: n.x + CARD_W, y: n.y + PORT_Y } : { x: n.x, y: n.y + PORT_Y };
  }

  function curve(a, b) {
    const dx = Math.max(56, Math.abs(b.x - a.x) * 0.5) + (b.x < a.x ? 80 : 0);
    return { p0: a, p1: { x: a.x + dx, y: a.y }, p2: { x: b.x - dx, y: b.y }, p3: b };
  }

  function curveD(c) {
    return `M${c.p0.x},${c.p0.y} C${c.p1.x},${c.p1.y} ${c.p2.x},${c.p2.y} ${c.p3.x},${c.p3.y}`;
  }

  function curveMid(c) {
    return {
      x: 0.125 * c.p0.x + 0.375 * c.p1.x + 0.375 * c.p2.x + 0.125 * c.p3.x,
      y: 0.125 * c.p0.y + 0.375 * c.p1.y + 0.375 * c.p2.y + 0.125 * c.p3.y,
    };
  }

  function makeEdge(edge) {
    const g = sv("g", { class: "edge e-pending" });
    const pid = `ep-${edge.id}`;
    const glow = sv("path", { class: "edge-glow", d: "" });
    const line = sv("path", { class: "edge-line", d: "", id: pid });
    const flow = sv("path", { class: "edge-flow", d: "" });
    const hit = sv("path", { class: "edge-hit", d: "" });
    g.appendChild(glow);
    g.appendChild(line);
    g.appendChild(flow);
    const parts = sv("g", { class: "edge-parts" });
    g.appendChild(parts);
    g.appendChild(hit);
    hit.addEventListener("pointerdown", (e) => {
      e.stopPropagation();
      select({ type: "edge", id: edge.id });
    });
    $("edge-layer").appendChild(g);
    const label = el("div", null, "edge-label e-pending");
    label.appendChild(el("span", null, "el-icon"));
    label.appendChild(el("span", "", "el-text"));
    label.addEventListener("pointerdown", (e) => {
      e.stopPropagation();
      select({ type: "edge", id: edge.id });
    });
    $("labels").appendChild(label);
    const ee = { g, line, glow, flow, hit, parts, label, pid, partsFor: null };
    S.edgeEls.set(edge.id, ee);
    placeEdge(edge);
    drawEdgeState(edge, S.status && S.status.edges ? S.status.edges[edge.id] : null);
    return ee;
  }

  function placeEdge(edge) {
    const ee = S.edgeEls.get(edge.id);
    if (!ee) return;
    const c = curve(portPos(edge.from, "out"), portPos(edge.to, "in"));
    const d = curveD(c);
    for (const p of [ee.line, ee.glow, ee.flow, ee.hit]) p.setAttribute("d", d);
    const m = curveMid(c);
    ee.label.style.left = `${m.x}px`;
    ee.label.style.top = `${m.y}px`;
  }

  function drawEdgeState(edge, es) {
    const ee = S.edgeEls.get(edge.id);
    if (!ee) return;
    const state = es ? es.state : "pending";
    const cls = `e-${["live", "warn", "error"].includes(state) ? state : "pending"}`;
    const isSel = Boolean(S.sel && S.sel.type === "edge" && S.sel.id === edge.id);
    ee.g.setAttribute("class", `edge ${cls}${isSel ? " sel" : ""}`);
    ee.label.className = `edge-label ${cls}${isSel ? " sel" : ""}`;
    const conn = connectionFor((nodeById(edge.from) || {}).kind, (nodeById(edge.to) || {}).kind);
    const text = (es && es.label) || (conn && conn.label) || "…";
    ee.label.lastChild.textContent = text;
    const issues = (es && es.issues) || [];
    ee.label.title = [es && es.writes ? `writes ${es.writes}` : null, ...issues.map((i) => `${i.level}: ${i.message}`)].filter(Boolean).join("\n");
    // particles only on a live edge (and not with reduced motion)
    const wantParts = state === "live" && !REDUCED.matches;
    if (wantParts && ee.partsFor !== "live") {
      clear(ee.parts);
      const durs = [1.9, 1.9, 1.9];
      durs.forEach((dur, i) => {
        const c = sv("circle", { r: i === 0 ? 3.2 : 2.2, class: `particle p${i}` });
        const am = sv("animateMotion", { dur: `${dur}s`, repeatCount: "indefinite", begin: `${-(dur / durs.length) * i}s`, rotate: "auto" });
        const mp = sv("mpath", {});
        mp.setAttributeNS("http://www.w3.org/1999/xlink", "xlink:href", `#${ee.pid}`);
        mp.setAttribute("href", `#${ee.pid}`);
        am.appendChild(mp);
        c.appendChild(am);
        ee.parts.appendChild(c);
      });
      ee.partsFor = "live";
    } else if (!wantParts && ee.partsFor) {
      clear(ee.parts);
      ee.partsFor = null;
    }
  }

  function spark(edgeId) {
    if (REDUCED.matches) return;
    const edge = edgeById(edgeId);
    if (!edge) return;
    for (const p of [portPos(edge.from, "out"), portPos(edge.to, "in")]) {
      const s = el("div", null, "spark");
      s.style.left = `${p.x}px`;
      s.style.top = `${p.y}px`;
      for (let i = 0; i < 8; i++) {
        const ray = el("i");
        ray.style.transform = `rotate(${i * 45 + Math.random() * 20}deg)`;
        s.appendChild(ray);
      }
      $("fx").appendChild(s);
      setTimeout(() => s.remove(), 760);
    }
    const ee = S.edgeEls.get(edgeId);
    if (ee) {
      ee.g.classList.add("surge");
      setTimeout(() => ee.g.classList.remove("surge"), 900);
    }
  }

  function refreshEdgesOf(nodeId) {
    for (const e of S.bp.edges) if (e.from === nodeId || e.to === nodeId) placeEdge(e);
  }

  // ---- draw everything -----------------------------------------------------

  function drawAll() {
    clear($("cards"));
    clear($("edge-layer"));
    clear($("labels"));
    S.nodeEls.clear();
    S.edgeEls.clear();
    for (const n of S.bp.nodes) makeCard(n, false);
    for (const e of S.bp.edges) makeEdge(e);
    // card heights are known only after layout
    requestAnimationFrame(() => {
      for (const e of S.bp.edges) placeEdge(e);
      drawMinimap();
    });
    drawEmptyHint();
    drawPaletteState();
  }

  function drawEmptyHint() {
    $("empty-hint").hidden = S.bp.nodes.some((n) => n.kind !== "sandbox");
  }

  // ---- palette -------------------------------------------------------------

  const collapsed = new Set();

  function drawPalette() {
    const q = $("pal-search").value.trim().toLowerCase();
    const box = clear($("pal-groups"));
    const groups = new Map();
    for (const it of S.palette.items || []) {
      const hay = `${it.label} ${it.description || ""} ${it.group || ""} ${it.kind}`.toLowerCase();
      if (q && !hay.includes(q)) continue;
      const g = it.group || "Other";
      if (!groups.has(g)) groups.set(g, []);
      groups.get(g).push(it);
    }
    if (!groups.size) box.appendChild(el("p", "Nothing matches.", "muted pal-empty"));
    for (const [g, items] of groups) {
      const sec = el("section", null, "pal-group");
      const h = el("button", null, "pal-gh");
      h.type = "button";
      const open = q || !collapsed.has(g);
      h.setAttribute("aria-expanded", String(Boolean(open)));
      h.appendChild(el("span", g, "pal-gname"));
      h.appendChild(el("span", String(items.length), "pal-gcount"));
      h.addEventListener("click", () => {
        if (collapsed.has(g)) collapsed.delete(g);
        else collapsed.add(g);
        drawPalette();
      });
      sec.appendChild(h);
      if (open) {
        const list = el("div", null, "pal-items");
        for (const it of items) list.appendChild(paletteItem(it));
        sec.appendChild(list);
      }
      box.appendChild(sec);
    }
    drawPaletteState();
  }

  function paletteItem(it) {
    const k = kindOf(it.kind);
    const b = el("button", null, `pal-item ${colorClass(k.color)}`);
    b.type = "button";
    b.dataset.item = it.id;
    b.dataset.kind = it.kind;
    const ic = el("span", null, "pal-icon");
    ic.appendChild(glyph(k.icon));
    b.appendChild(ic);
    const tx = el("span", null, "pal-text");
    tx.appendChild(el("span", it.label, "pal-label"));
    if (it.description) tx.appendChild(el("span", it.description, "pal-desc"));
    b.appendChild(tx);
    if (it.badges && it.badges.length) {
      const bs = el("span", null, "pal-badges");
      for (const bd of it.badges) bs.appendChild(el("span", bd, "badge-mini"));
      b.appendChild(bs);
    }
    b.title = it.disabled ? `Not in this build: ${it.disabled}` : it.description || it.label;
    if (it.disabled) b.dataset.disabled = "1";
    b.addEventListener("pointerdown", (e) => startPaletteDrag(e, it));
    b.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        dropItemCentre(it);
      }
    });
    return b;
  }

  function singletonTaken(kind) {
    return kindOf(kind).singleton && S.bp.nodes.some((n) => n.kind === kind);
  }

  function drawPaletteState() {
    for (const b of document.querySelectorAll(".pal-item")) {
      const off = !S.editable || singletonTaken(b.dataset.kind) || b.dataset.disabled === "1";
      b.classList.toggle("off", off);
      b.setAttribute("aria-disabled", String(off));
    }
  }

  function startPaletteDrag(e, it) {
    if (e.button !== 0) return;
    // Rust says why this card cannot be used in this build (`Item.disabled`).
    if (it.disabled) {
      toast(`${it.label}: ${it.disabled}`, "warn");
      return;
    }
    if (!S.editable || singletonTaken(it.kind)) {
      if (singletonTaken(it.kind)) toast(`Only one ${kindOf(it.kind).label} per sandbox.`, "warn");
      return;
    }
    e.preventDefault();
    const start = { x: e.clientX, y: e.clientY };
    const ghost = $("ghost");
    let dragging = false;
    const move = (ev) => {
      if (!dragging && Math.hypot(ev.clientX - start.x, ev.clientY - start.y) > DRAG_PX) {
        dragging = true;
        const k = kindOf(it.kind);
        clear(ghost);
        ghost.className = `ghost ${colorClass(k.color)}`;
        const ic = el("span", null, "card-icon");
        ic.appendChild(glyph(k.icon));
        ghost.appendChild(ic);
        ghost.appendChild(el("span", it.label, "ghost-label"));
        ghost.hidden = false;
        $("stage").classList.add("dropping");
      }
      if (dragging) {
        ghost.style.left = `${ev.clientX}px`;
        ghost.style.top = `${ev.clientY}px`;
        const over = overStage(ev.clientX, ev.clientY);
        ghost.classList.toggle("over", over);
      }
    };
    const up = (ev) => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      ghost.hidden = true;
      $("stage").classList.remove("dropping");
      if (!dragging) {
        dropItemCentre(it);
        return;
      }
      if (overStage(ev.clientX, ev.clientY)) {
        const w = toWorld(ev.clientX, ev.clientY);
        addNode(it, w.x - CARD_W / 2, w.y - PORT_Y);
      }
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  }

  function overStage(x, y) {
    const r = $("stage").getBoundingClientRect();
    if (x < r.left || x > r.right || y < r.top || y > r.bottom) return false;
    const hit = document.elementFromPoint(x, y);
    return Boolean(hit && $("stage").contains(hit) && !hit.closest(".zoom-ctl, .minimap"));
  }

  function dropItemCentre(it) {
    if (!S.editable) return;
    if (it.disabled) {
      toast(`${it.label}: ${it.disabled}`, "warn");
      return;
    }
    if (singletonTaken(it.kind)) {
      toast(`Only one ${kindOf(it.kind).label} per sandbox.`, "warn");
      return;
    }
    const r = $("stage").getBoundingClientRect();
    const w = toWorld(r.left + r.width / 2, r.top + r.height / 2);
    const p = freeSpot(w.x - CARD_W / 2, w.y - PORT_Y);
    addNode(it, p.x, p.y);
  }

  // the nearest place around (x, y) where a new card overlaps no other card
  function freeSpot(x, y) {
    const H = 92;
    const clash = (px, py) =>
      S.bp.nodes.some((n) => px < n.x + CARD_W + 24 && px + CARD_W + 24 > n.x && py < n.y + cardHeight(n.id) + 20 && py + H + 20 > n.y);
    if (!clash(x, y)) return { x, y };
    for (let ring = 1; ring < 14; ring++) {
      for (let i = 0; i < ring * 8; i++) {
        const a = (i / (ring * 8)) * Math.PI * 2;
        const px = x + Math.cos(a) * ring * 130;
        const py = y + Math.sin(a) * ring * 75;
        if (!clash(px, py)) return { x: px, y: py };
      }
    }
    return { x: x + 40, y: y + 40 };
  }

  function addNode(it, x, y) {
    snapshot();
    const k = kindOf(it.kind);
    const fields = {};
    for (const f of k.fields || []) if (f.default !== undefined && f.default !== null) fields[f.key] = clone(f.default);
    Object.assign(fields, clone(it.preset || {}));
    const node = { id: `n-${it.kind}-${rnd(6)}`, kind: it.kind, x: Math.round(x), y: Math.round(y), fields };
    S.bp.nodes.push(node);
    makeCard(node, true);
    drawEmptyHint();
    drawPaletteState();
    select({ type: "node", id: node.id }, true);
    save();
  }

  // ---- card drag + selection -----------------------------------------------

  function startCardDrag(e, id) {
    if (e.button !== 0 || e.target.closest(".port")) return;
    e.stopPropagation();
    const node = nodeById(id);
    if (!node) return;
    const start = { x: e.clientX, y: e.clientY, nx: node.x, ny: node.y };
    let moved = false;
    const z = view().zoom;
    const ne = S.nodeEls.get(id);
    const move = (ev) => {
      const dx = (ev.clientX - start.x) / z;
      const dy = (ev.clientY - start.y) / z;
      if (!moved && Math.hypot(dx, dy) * z < DRAG_PX) return;
      if (!S.editable) return;
      if (!moved) {
        moved = true;
        snapshot();
        ne.card.classList.add("dragging");
      }
      node.x = Math.round(start.nx + dx);
      node.y = Math.round(start.ny + dy);
      placeCard(node);
      refreshEdgesOf(id);
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      ne.card.classList.remove("dragging");
      if (moved) {
        drawMinimap();
        save();
      } else {
        select({ type: "node", id });
      }
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  }

  function select(sel, focusFirst) {
    S.sel = sel;
    for (const [id, ne] of S.nodeEls) ne.card.classList.toggle("sel", Boolean(sel && sel.type === "node" && sel.id === id));
    for (const e of S.bp.edges) drawEdgeState(e, S.status && S.status.edges ? S.status.edges[e.id] : null);
    drawInspector(focusFirst);
    if (window.innerWidth < 1100) $("inspector").classList.toggle("open", Boolean(sel));
  }

  function focusOn(sel) {
    select(sel);
    let p = null;
    if (sel.type === "node") {
      const n = nodeById(sel.id);
      if (n) p = { x: n.x + CARD_W / 2, y: n.y + 40 };
    } else {
      const e = edgeById(sel.id);
      if (e) {
        const c = curve(portPos(e.from, "out"), portPos(e.to, "in"));
        p = curveMid(c);
      }
    }
    if (!p) return;
    const r = $("stage").getBoundingClientRect();
    const v = view();
    v.x = Math.round(r.width / 2 - p.x * v.zoom);
    v.y = Math.round(r.height / 2 - p.y * v.zoom);
    $("world").classList.add("glide");
    applyView();
    setTimeout(() => $("world").classList.remove("glide"), 420);
    const ne = sel.type === "node" ? S.nodeEls.get(sel.id) : null;
    if (ne && !REDUCED.matches) {
      ne.card.classList.add("flash");
      setTimeout(() => ne.card.classList.remove("flash"), 900);
    }
    saveViewSoon();
  }

  function deleteSelected() {
    if (!S.sel || !S.editable) return;
    if (S.sel.type === "node") {
      const n = nodeById(S.sel.id);
      if (!n) return;
      const k = kindOf(n.kind);
      if (k.deletable === false || n.kind === "sandbox") {
        toast(`The ${k.label} card stays: every sandbox has one.`, "warn");
        return;
      }
      snapshot();
      S.bp.nodes = S.bp.nodes.filter((x) => x.id !== n.id);
      S.bp.edges = S.bp.edges.filter((e) => e.from !== n.id && e.to !== n.id);
    } else {
      snapshot();
      S.bp.edges = S.bp.edges.filter((e) => e.id !== S.sel.id);
    }
    S.sel = null;
    drawAll();
    drawInspector();
    save();
  }

  // ---- connecting ----------------------------------------------------------

  function startConnect(e, fromId) {
    if (e.button !== 0) return;
    e.stopPropagation();
    e.preventDefault();
    if (!S.editable) return;
    const from = nodeById(fromId);
    if (!from) return;
    const okKinds = new Set(targetsOf(from.kind).map((c) => c.to));
    const stage = $("stage");
    stage.classList.add("connecting");
    for (const n of S.bp.nodes) {
      const ne = S.nodeEls.get(n.id);
      if (!ne || n.id === fromId) continue;
      const dup = S.bp.edges.some((x) => x.from === fromId && x.to === n.id);
      ne.card.classList.add(okKinds.has(n.kind) && !dup ? "drop-ok" : "drop-dim");
    }
    S.nodeEls.get(fromId).card.classList.add("drop-src");
    const path = $("drag-edge");
    path.setAttribute("class", "drag-edge on");
    const a = portPos(fromId, "out");
    let hover = null;
    const move = (ev) => {
      const w = toWorld(ev.clientX, ev.clientY);
      const card = cardAt(ev.clientX, ev.clientY);
      if (hover && hover !== card) hover.classList.remove("drop-hover");
      hover = card && card.classList.contains("drop-ok") ? card : null;
      if (hover) hover.classList.add("drop-hover");
      const b = hover ? portPos(hover.dataset.id, "in") : w;
      path.setAttribute("d", curveD(curve(a, b)));
      path.setAttribute("class", `drag-edge on${hover ? " snap" : ""}`);
    };
    const up = (ev) => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      path.setAttribute("class", "drag-edge");
      path.setAttribute("d", "");
      stage.classList.remove("connecting");
      for (const ne of S.nodeEls.values()) ne.card.classList.remove("drop-ok", "drop-dim", "drop-hover", "drop-src");
      const card = cardAt(ev.clientX, ev.clientY);
      if (!card || card.dataset.id === fromId) return;
      connect(fromId, card.dataset.id, card);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    move(e);
  }

  function cardAt(x, y) {
    const hit = document.elementFromPoint(x, y);
    return hit ? hit.closest(".card") : null;
  }

  function connect(fromId, toId, card) {
    const from = nodeById(fromId);
    const to = nodeById(toId);
    if (!from || !to) return;
    if (S.bp.edges.some((x) => x.from === fromId && x.to === toId)) {
      toast("Already connected.", "warn");
      return;
    }
    const conn = connectionFor(from.kind, to.kind);
    if (!conn) {
      const fk = kindOf(from.kind).label;
      const tk = kindOf(to.kind).label;
      const can = targetsOf(from.kind).map((c) => kindOf(c.to).label);
      if (card && !REDUCED.matches) {
        card.classList.add("deny");
        setTimeout(() => card.classList.remove("deny"), 650);
      }
      toast(`No connection ${fk} → ${tk}.${can.length ? ` ${fk} connects to: ${[...new Set(can)].join(", ")}.` : ""}`, "error");
      return;
    }
    snapshot();
    const edge = { id: `e-${rnd(8)}`, from: fromId, to: toId };
    S.bp.edges.push(edge);
    makeEdge(edge);
    select({ type: "edge", id: edge.id });
    save();
  }

  // ---- pan / zoom on the stage ---------------------------------------------

  function wireStage() {
    const stage = $("stage");
    stage.addEventListener("pointerdown", (e) => {
      if (e.target.closest(".card, .zoom-ctl, .minimap, .edge-label")) return;
      if (e.button !== 0 && e.button !== 1) return;
      const start = { x: e.clientX, y: e.clientY };
      const v = view();
      const v0 = { x: v.x, y: v.y };
      let moved = false;
      stage.classList.add("panning");
      const move = (ev) => {
        const dx = ev.clientX - start.x;
        const dy = ev.clientY - start.y;
        if (!moved && Math.hypot(dx, dy) < DRAG_PX) return;
        moved = true;
        v.x = Math.round(v0.x + dx);
        v.y = Math.round(v0.y + dy);
        applyView();
      };
      const up = () => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", up);
        stage.classList.remove("panning");
        if (moved) saveViewSoon();
        else if (S.sel) select(null);
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", up);
    });
    stage.addEventListener(
      "wheel",
      (e) => {
        e.preventDefault();
        if (e.ctrlKey || e.metaKey || Math.abs(e.deltaY) >= Math.abs(e.deltaX)) {
          const f = Math.exp(-e.deltaY * (e.ctrlKey ? 0.01 : 0.0015));
          zoomAt(e.clientX, e.clientY, f);
        } else {
          const v = view();
          v.x -= Math.round(e.deltaX);
          applyView();
          saveViewSoon();
        }
      },
      { passive: false },
    );
    $("z-in").addEventListener("click", () => {
      const r = stage.getBoundingClientRect();
      zoomAt(r.left + r.width / 2, r.top + r.height / 2, 1.2);
    });
    $("z-out").addEventListener("click", () => {
      const r = stage.getBoundingClientRect();
      zoomAt(r.left + r.width / 2, r.top + r.height / 2, 1 / 1.2);
    });
    $("z-fit").addEventListener("click", fit);
    $("minimap").addEventListener("pointerdown", (e) => {
      e.stopPropagation();
      const mm = S.mini;
      if (!mm) return;
      const r = $("minimap").getBoundingClientRect();
      const wx = mm.x0 + (e.clientX - r.left) / mm.s;
      const wy = mm.y0 + (e.clientY - r.top) / mm.s;
      const sr = stage.getBoundingClientRect();
      const v = view();
      v.x = Math.round(sr.width / 2 - wx * v.zoom);
      v.y = Math.round(sr.height / 2 - wy * v.zoom);
      $("world").classList.add("glide");
      applyView();
      setTimeout(() => $("world").classList.remove("glide"), 420);
      saveViewSoon();
    });
  }

  // ---- minimap -------------------------------------------------------------

  function drawMinimap() {
    const mm = $("minimap");
    clear(mm);
    const b = bounds();
    const sr = $("stage").getBoundingClientRect();
    if (!b || !sr.width) {
      mm.classList.add("empty");
      S.mini = null;
      return;
    }
    mm.classList.remove("empty");
    const v = view();
    const vis = { x0: -v.x / v.zoom, y0: -v.y / v.zoom, x1: (sr.width - v.x) / v.zoom, y1: (sr.height - v.y) / v.zoom };
    const x0 = Math.min(b.x0, vis.x0) - 40;
    const y0 = Math.min(b.y0, vis.y0) - 40;
    const x1 = Math.max(b.x1, vis.x1) + 40;
    const y1 = Math.max(b.y1, vis.y1) + 40;
    const W = 176;
    const H = 116;
    const s = Math.min(W / (x1 - x0), H / (y1 - y0));
    S.mini = { x0, y0, s };
    mm.setAttribute("viewBox", `0 0 ${W} ${H}`);
    for (const e of S.bp.edges) {
      const a = portPos(e.from, "out");
      const c = portPos(e.to, "in");
      const st = S.edgeState.get(e.id) || "pending";
      mm.appendChild(sv("line", { x1: (a.x - x0) * s, y1: (a.y - y0) * s, x2: (c.x - x0) * s, y2: (c.y - y0) * s, class: `mm-edge mm-${st}` }));
    }
    for (const n of S.bp.nodes) {
      const st = S.status && S.status.nodes && S.status.nodes[n.id] ? S.status.nodes[n.id].state : "pending";
      mm.appendChild(sv("rect", { x: (n.x - x0) * s, y: (n.y - y0) * s, width: Math.max(3, CARD_W * s), height: Math.max(2, cardHeight(n.id) * s), rx: 1.5, class: `mm-node mm-${st} ${colorClass(kindOf(n.kind).color)}` }));
    }
    mm.appendChild(sv("rect", { x: (vis.x0 - x0) * s, y: (vis.y0 - y0) * s, width: (vis.x1 - vis.x0) * s, height: (vis.y1 - vis.y0) * s, class: "mm-view" }));
  }

  // ---- inspector -----------------------------------------------------------

  function nodeIssues(id) {
    const st = S.status && S.status.nodes && S.status.nodes[id];
    return (st && st.issues) || [];
  }

  function edgeStatus(id) {
    return (S.status && S.status.edges && S.status.edges[id]) || null;
  }

  function issueList(issues, cls) {
    const ul = el("ul", null, `issues ${cls || ""}`);
    for (const i of issues) {
      const li = el("li", null, `issue i-${i.level === "error" ? "error" : "warn"}`);
      li.appendChild(el("span", i.level === "error" ? "error" : "warning", "i-level"));
      li.appendChild(el("span", i.message, "i-msg"));
      ul.appendChild(li);
    }
    return ul;
  }

  function drawInspector(focusFirst) {
    const box = clear($("inspector"));
    S.fieldIssueEls.clear();
    if (!S.bp) return;
    if (!S.sel) return drawOverview(box);
    if (S.sel.type === "node") {
      const n = nodeById(S.sel.id);
      if (!n) return drawOverview(box);
      return drawNodeInspector(box, n, focusFirst);
    }
    const e = edgeById(S.sel.id);
    if (!e) return drawOverview(box);
    return drawEdgeInspector(box, e);
  }

  function inspHead(box, iconName, color, kindLabel, title, id) {
    const h = el("div", null, `insp-head ${colorClass(color)}`);
    const ic = el("span", null, "card-icon big");
    ic.appendChild(glyph(iconName));
    h.appendChild(ic);
    const t = el("div", null, "insp-titles");
    t.appendChild(el("span", kindLabel, "card-kind"));
    t.appendChild(el("h2", title, "insp-title"));
    if (id) t.appendChild(el("span", id, "id muted"));
    h.appendChild(t);
    const x = el("button", "×", "icon-btn insp-close");
    x.type = "button";
    x.title = "Deselect (Esc)";
    x.addEventListener("click", () => select(null));
    h.appendChild(x);
    box.appendChild(h);
  }

  function drawOverview(box) {
    const h = el("div", null, "insp-head c-slate");
    const ic = el("span", null, "card-icon big");
    ic.appendChild(glyph("sandbox"));
    h.appendChild(ic);
    const t = el("div", null, "insp-titles");
    t.appendChild(el("span", "Sandbox", "card-kind"));
    t.appendChild(el("h2", S.sandbox, "insp-title"));
    t.appendChild(el("span", `sandboxes/${S.sandbox}/builder.json`, "id muted"));
    h.appendChild(t);
    box.appendChild(h);

    const counts = new Map();
    for (const n of S.bp.nodes) counts.set(n.kind, (counts.get(n.kind) || 0) + 1);
    const grid = el("div", null, "stat-grid");
    for (const [k, c] of counts) {
      const s = el("div", null, `stat ${colorClass(kindOf(k).color)}`);
      s.appendChild(el("span", String(c), "stat-n"));
      s.appendChild(el("span", kindOf(k).label, "stat-l"));
      grid.appendChild(s);
    }
    const se = el("div", null, "stat c-green");
    se.appendChild(el("span", String(S.bp.edges.length), "stat-n"));
    se.appendChild(el("span", "connections", "stat-l"));
    grid.appendChild(se);
    box.appendChild(grid);

    box.appendChild(el("h3", "Issues"));
    const issues = (S.status && S.status.issues) || [];
    if (!issues.length) box.appendChild(el("p", S.status ? "None — Rust accepts this blueprint." : "Waiting for the first check…", "muted"));
    else {
      const ul = el("ul", null, "issues clickable");
      for (const i of issues) {
        const li = el("li", null, `issue i-${i.level === "error" ? "error" : "warn"}`);
        li.appendChild(el("span", i.level === "error" ? "error" : "warning", "i-level"));
        const where = i.node ? cardTitle(nodeById(i.node) || { kind: "", fields: {}, id: i.node }) : i.edge ? "connection" : "sandbox";
        li.appendChild(el("span", `${where}${i.field ? ` · ${i.field}` : ""}`, "i-where"));
        li.appendChild(el("span", i.message, "i-msg"));
        if (i.node || i.edge) {
          li.tabIndex = 0;
          li.addEventListener("click", () => focusOn(i.node ? { type: "node", id: i.node } : { type: "edge", id: i.edge }));
        }
        ul.appendChild(li);
      }
      box.appendChild(ul);
    }

    box.appendChild(el("h3", "Connections"));
    const lg = el("ul", null, "legend");
    for (const [cls, name, what] of [
      ["live", "live", "Rust compiled it: power flows"],
      ["warn", "warning", "compiles, but check the note"],
      ["error", "error", "Rust refuses it as drawn"],
      ["pending", "pending", "not checked yet"],
    ]) {
      const li = el("li");
      const sw = sv("svg", { viewBox: "0 0 44 10", class: `legend-sw e-${cls}` });
      sw.appendChild(sv("path", { d: "M2,5 C14,0 30,10 42,5", class: "edge-line" }));
      li.appendChild(sw);
      li.appendChild(el("strong", name));
      li.appendChild(el("span", what, "muted"));
      lg.appendChild(li);
    }
    box.appendChild(lg);

    if (S.palette.engines && S.palette.engines.length) {
      box.appendChild(el("h3", "Engines"));
      const ul = el("ul", null, "plain-list");
      for (const en of S.palette.engines) {
        const li = el("li");
        li.appendChild(el("strong", en.label));
        li.appendChild(el("span", ` ${en.id}`, "id muted"));
        if (en.note) li.appendChild(el("div", en.note, "muted small"));
        ul.appendChild(li);
      }
      box.appendChild(ul);
    }
    if (S.palette.secret_backends && S.palette.secret_backends.length) {
      box.appendChild(el("h3", "Secret backends"));
      const ul = el("ul", null, "plain-list");
      for (const sb of S.palette.secret_backends) {
        const li = el("li");
        li.appendChild(el("span", sb.available ? "available" : "unavailable", `chip ${sb.available ? "ch-ok" : "ch-off"}`));
        li.appendChild(el("strong", ` ${sb.label}`));
        if (sb.note) li.appendChild(el("div", sb.note, "muted small"));
        ul.appendChild(li);
      }
      box.appendChild(ul);
    }

    box.appendChild(el("h3", "Keys"));
    const keys = el("dl", null, "keys");
    for (const [k, v] of [
      ["Drag", "palette → canvas · card → move · empty space → pan"],
      ["Connect", "drag from a card's right port onto another card"],
      ["Del / ⌫", "delete the selection"],
      ["⌘/Ctrl Z", "undo · ⇧ redo"],
      ["⌘/Ctrl S", "save now"],
      ["⌘/Ctrl ↵", "validate"],
      ["F", "fit · Esc deselect"],
    ]) {
      keys.appendChild(el("dt", k));
      keys.appendChild(el("dd", v));
    }
    box.appendChild(keys);
  }

  function visibleField(f, fields) {
    if (!f.show_if) return true;
    const v = fields[f.show_if.field];
    const list = f.show_if.in || [];
    return list.includes(v) || list.includes(String(v));
  }

  function suggestions(f, fields) {
    if (f.suggest_from && f.suggest_from.map) {
      const key = fields[f.suggest_from.field];
      const s = f.suggest_from.map[key];
      if (s && s.length) return s;
    }
    return f.suggest || [];
  }

  function drawNodeInspector(box, n, focusFirst) {
    const k = kindOf(n.kind);
    inspHead(box, k.icon, k.color, k.label, cardTitle(n), n.id);
    const issues = nodeIssues(n.id);
    const fieldKeys = new Set((k.fields || []).filter((f) => visibleField(f, n.fields)).map((f) => f.key));
    const top = el("div", null, "insp-top-issues");
    top.appendChild(issueList(issues.filter((i) => !i.field || !fieldKeys.has(i.field))));
    S.fieldIssueEls.set("", top);
    box.appendChild(top);
    if (k.help) box.appendChild(el("p", k.help, "insp-help"));

    const item = itemFor(n);
    if (item && item.description) {
      const d = el("div", null, "tool-desc");
      d.appendChild(el("p", item.description));
      box.appendChild(d);
    }

    const form = el("div", null, "form");
    let firstEmpty = null;
    const rows = new Map();
    const relayout = () => {
      for (const f of k.fields || []) {
        const r = rows.get(f.key);
        if (r) r.row.hidden = !visibleField(f, n.fields);
        if (r && r.datalist) fillDatalist(r.datalist, suggestions(f, n.fields));
      }
    };
    for (const f of k.fields || []) {
      const row = fieldRow(n, f, relayout);
      rows.set(f.key, row);
      row.row.hidden = !visibleField(f, n.fields);
      form.appendChild(row.row);
      const v = n.fields[f.key];
      if (!firstEmpty && f.required && !f.readonly && !row.row.hidden && (v === undefined || v === "" || (Array.isArray(v) && !v.length)) && row.input) firstEmpty = row.input;
    }
    box.appendChild(form);
    refreshInspectorIssues();
    if (focusFirst && firstEmpty && S.editable) setTimeout(() => firstEmpty.focus(), 60);

    if (item && item.parameters) box.appendChild(paramsView(item.parameters));

    // connections of this node
    const mine = S.bp.edges.filter((e) => e.from === n.id || e.to === n.id);
    box.appendChild(el("h3", `Connections (${mine.length})`));
    if (!mine.length) {
      const can = targetsOf(n.kind).map((c) => kindOf(c.to).label);
      box.appendChild(el("p", can.length ? `Drag from the right port to: ${[...new Set(can)].join(", ")}.` : "Other cards connect to this one.", "muted"));
    } else {
      const ul = el("ul", null, "conn-list");
      for (const e of mine) {
        const es = edgeStatus(e.id);
        const other = nodeById(e.from === n.id ? e.to : e.from);
        const li = el("li", null, `conn e-${es ? es.state : "pending"}`);
        li.tabIndex = 0;
        li.appendChild(el("span", e.from === n.id ? "→" : "←", "conn-dir"));
        const t = el("div", null, "conn-text");
        t.appendChild(el("span", `${(es && es.label) || "…"} ${other ? cardTitle(other) : e.to}`, "conn-main"));
        if (es && es.writes) t.appendChild(el("code", es.writes, "conn-writes"));
        li.appendChild(t);
        li.appendChild(el("span", es ? es.state : "pending", `chip ch-${es ? es.state : "pending"}`));
        li.addEventListener("click", () => select({ type: "edge", id: e.id }));
        ul.appendChild(li);
      }
      box.appendChild(ul);
    }

    const del = el("button", `Delete ${k.label.toLowerCase()}`, "danger wide");
    del.type = "button";
    del.disabled = !S.editable || k.deletable === false || n.kind === "sandbox";
    if (n.kind === "sandbox" || k.deletable === false) del.title = "Every sandbox keeps this card";
    del.addEventListener("click", deleteSelected);
    box.appendChild(del);
  }

  function fillDatalist(dl, values) {
    clear(dl);
    for (const v of values || []) {
      const o = el("option");
      o.value = v;
      dl.appendChild(o);
    }
  }

  function fieldRow(n, f, relayout) {
    const row = el("div", null, `field f-${f.type}`);
    const id = `fld-${n.id}-${f.key}`;
    const lab = el("label", null, "field-label");
    lab.htmlFor = id;
    lab.appendChild(el("span", f.label || f.key));
    if (f.required) lab.appendChild(el("span", "*", "req"));
    lab.appendChild(el("code", f.key, "field-key"));
    row.appendChild(lab);
    const ro = f.readonly || !S.editable;
    let input = null;
    let datalist = null;

    const set = (value) => {
      const before = JSON.stringify(n.fields[f.key]);
      if (value === undefined || value === null || value === "" || (Array.isArray(value) && !value.length)) delete n.fields[f.key];
      else n.fields[f.key] = value;
      if (JSON.stringify(n.fields[f.key]) === before) return;
      if (!S.editBurst) {
        // one undo step per burst of typing
        S.undo.push(S.pendingSnap || JSON.stringify({ nodes: S.bp.nodes, edges: S.bp.edges }));
        if (S.undo.length > UNDO_MAX) S.undo.shift();
        S.redo.length = 0;
        drawUndo();
      }
      clearTimeout(S.editBurst);
      S.editBurst = setTimeout(() => {
        S.editBurst = null;
      }, 1200);
      drawCardState(n);
      relayout();
      saveSoon();
    };
    const snapBefore = () => {
      if (!S.editBurst) S.pendingSnap = JSON.stringify({ nodes: S.bp.nodes, edges: S.bp.edges });
    };

    const v = n.fields[f.key];
    switch (f.type) {
      case "text": {
        input = el("textarea");
        input.rows = 3;
        input.value = v === undefined ? "" : String(v);
        if (f.placeholder) input.placeholder = f.placeholder;
        input.addEventListener("focus", snapBefore);
        input.addEventListener("input", () => {
          autoGrow(input);
          set(input.value);
        });
        requestAnimationFrame(() => autoGrow(input));
        break;
      }
      case "int": {
        input = el("input");
        input.type = "number";
        input.step = "1";
        if (f.min !== undefined && f.min !== null) input.min = String(f.min);
        if (f.max !== undefined && f.max !== null) input.max = String(f.max);
        input.value = v === undefined ? "" : String(v);
        if (f.placeholder) input.placeholder = f.placeholder;
        input.addEventListener("focus", snapBefore);
        input.addEventListener("input", () => {
          const t = input.value.trim();
          if (t === "") return set(undefined);
          const num = Number(t);
          if (Number.isInteger(num)) set(num);
        });
        break;
      }
      case "bool": {
        const wrap = el("label", null, "switch");
        input = el("input");
        input.type = "checkbox";
        input.checked = Boolean(v);
        input.addEventListener("change", () => {
          snapBefore();
          set(input.checked);
        });
        wrap.appendChild(input);
        wrap.appendChild(el("span", null, "switch-track"));
        wrap.appendChild(el("span", input.checked ? "on" : "off", "switch-text"));
        input.addEventListener("change", () => {
          wrap.lastChild.textContent = input.checked ? "on" : "off";
        });
        row.appendChild(wrap);
        break;
      }
      case "select": {
        input = el("select");
        if (!f.required) {
          const o = el("option", "—");
          o.value = "";
          input.appendChild(o);
        }
        const opts = f.options || [];
        for (const op of opts) {
          const o = el("option", op.label || op.value);
          o.value = op.value;
          input.appendChild(o);
        }
        if (v !== undefined && !opts.some((o) => o.value === v)) {
          const o = el("option", `${v} (unknown)`);
          o.value = v;
          input.appendChild(o);
        }
        input.value = v === undefined ? "" : String(v);
        input.addEventListener("change", () => {
          snapBefore();
          set(input.value === "" ? undefined : input.value);
        });
        break;
      }
      case "list": {
        const box = el("div", null, "chips");
        const list = Array.isArray(v) ? v.slice() : [];
        input = el("input");
        input.type = "text";
        input.placeholder = f.placeholder || "type, then Enter";
        datalist = el("datalist");
        datalist.id = `${id}-dl`;
        input.setAttribute("list", datalist.id);
        fillDatalist(datalist, suggestions(f, n.fields));
        const draw = () => {
          for (const c of Array.from(box.querySelectorAll(".chip-v"))) c.remove();
          list.forEach((item, i) => {
            const c = el("span", null, "chip-v");
            c.appendChild(el("span", item));
            if (!ro) {
              const x = el("button", "×", "chip-x");
              x.type = "button";
              x.title = `Remove ${item}`;
              x.addEventListener("click", () => {
                snapBefore();
                list.splice(i, 1);
                draw();
                set(list.slice());
              });
              c.appendChild(x);
            }
            box.insertBefore(c, input);
          });
        };
        const add = () => {
          const parts = input.value.split(",").map((s) => s.trim()).filter(Boolean);
          if (!parts.length) return;
          snapBefore();
          for (const p of parts) if (!list.includes(p)) list.push(p);
          input.value = "";
          draw();
          set(list.slice());
        };
        input.addEventListener("keydown", (e) => {
          if (e.key === "Enter" || e.key === ",") {
            e.preventDefault();
            add();
          } else if (e.key === "Backspace" && !input.value && list.length && !ro) {
            snapBefore();
            list.pop();
            draw();
            set(list.slice());
          }
        });
        input.addEventListener("change", add);
        box.appendChild(input);
        box.appendChild(datalist);
        draw();
        box.addEventListener("click", (e) => {
          if (e.target === box) input.focus();
        });
        row.appendChild(box);
        break;
      }
      default: {
        input = el("input");
        input.type = f.secret ? "password" : "text";
        input.value = v === undefined ? "" : String(v);
        input.spellcheck = false;
        if (f.placeholder) input.placeholder = f.placeholder;
        const sugg = suggestions(f, n.fields);
        if (sugg.length || f.suggest_from) {
          datalist = el("datalist");
          datalist.id = `${id}-dl`;
          input.setAttribute("list", datalist.id);
          fillDatalist(datalist, sugg);
        }
        input.addEventListener("focus", snapBefore);
        input.addEventListener("input", () => set(input.value));
      }
    }
    if (input) {
      input.id = id;
      input.disabled = ro;
      if (f.type !== "bool" && f.type !== "list") {
        input.className = "field-input";
        row.appendChild(input);
        if (datalist && f.type !== "list") row.appendChild(datalist);
      }
    }
    if (f.help) row.appendChild(el("p", f.help, "field-help"));
    const iss = el("div", null, "field-issues");
    S.fieldIssueEls.set(f.key, iss);
    row.appendChild(iss);
    return { row, input, datalist };
  }

  function autoGrow(t) {
    t.style.height = "auto";
    t.style.height = `${Math.min(320, t.scrollHeight + 2)}px`;
  }

  // update issue containers in place (never rebuild a form mid-typing)
  function refreshInspectorIssues() {
    if (!S.sel) return;
    if (S.sel.type === "edge") {
      const e = edgeById(S.sel.id);
      if (!e) return;
      const box = $("inspector");
      if (box.contains(document.activeElement) && document.activeElement !== document.body) return;
      drawInspector();
      return;
    }
    const n = nodeById(S.sel.id);
    if (!n) return;
    const issues = nodeIssues(n.id);
    const shown = new Set();
    for (const [key, box] of S.fieldIssueEls) {
      if (key === "") continue;
      const row = box.parentElement;
      if (row && row.hidden) continue;
      const mine = issues.filter((i) => i.field === key);
      mine.forEach((i) => shown.add(i));
      clear(box);
      if (mine.length) box.appendChild(issueList(mine, "inline"));
      if (row) {
        row.classList.toggle("has-error", mine.some((i) => i.level === "error"));
        row.classList.toggle("has-warn", mine.length > 0 && !mine.some((i) => i.level === "error"));
      }
    }
    const top = S.fieldIssueEls.get("");
    if (top) {
      clear(top);
      const rest = issues.filter((i) => !shown.has(i));
      if (rest.length) top.appendChild(issueList(rest));
    }
    const t = $("inspector").querySelector(".insp-title");
    if (t) t.textContent = cardTitle(n);
    for (const li of $("inspector").querySelectorAll(".conn-list li")) li.remove();
    redrawConnList(n);
  }

  function redrawConnList(n) {
    const ul = $("inspector").querySelector(".conn-list");
    if (!ul) return;
    for (const e of S.bp.edges.filter((x) => x.from === n.id || x.to === n.id)) {
      const es = edgeStatus(e.id);
      const other = nodeById(e.from === n.id ? e.to : e.from);
      const li = el("li", null, `conn e-${es ? es.state : "pending"}`);
      li.tabIndex = 0;
      li.appendChild(el("span", e.from === n.id ? "→" : "←", "conn-dir"));
      const t = el("div", null, "conn-text");
      t.appendChild(el("span", `${(es && es.label) || "…"} ${other ? cardTitle(other) : e.to}`, "conn-main"));
      if (es && es.writes) t.appendChild(el("code", es.writes, "conn-writes"));
      li.appendChild(t);
      li.appendChild(el("span", es ? es.state : "pending", `chip ch-${es ? es.state : "pending"}`));
      li.addEventListener("click", () => select({ type: "edge", id: e.id }));
      ul.appendChild(li);
    }
  }

  function paramsView(schema) {
    const d = el("details", null, "params");
    d.appendChild(el("summary", "Call arguments (the tool's JSON schema)"));
    const tbl = el("table", null, "param-table");
    const thead = el("thead");
    const hr = el("tr");
    for (const h of ["name", "type", "", "description"]) hr.appendChild(el("th", h));
    thead.appendChild(hr);
    tbl.appendChild(thead);
    const tb = el("tbody");
    const walk = (sch, depth, prefix) => {
      const props = (sch && sch.properties) || {};
      const req = new Set((sch && sch.required) || []);
      for (const [name, p] of Object.entries(props)) {
        const tr = el("tr");
        const nm = el("td", null, "pname");
        nm.appendChild(el("code", `${prefix}${name}`));
        if (depth) nm.classList.add(`d${Math.min(depth, 3)}`);
        tr.appendChild(nm);
        let type = Array.isArray(p.type) ? p.type.join(" | ") : p.type || (p.enum ? "enum" : p.anyOf || p.oneOf ? "any of" : "");
        if (type === "array" && p.items && p.items.type) type = `${p.items.type}[]`;
        tr.appendChild(el("td", type, "ptype"));
        tr.appendChild(el("td", req.has(name) ? "req" : "", "preq"));
        const desc = [p.description || "", p.enum ? `one of: ${p.enum.join(", ")}` : "", p.default !== undefined ? `default ${JSON.stringify(p.default)}` : ""].filter(Boolean).join(" · ");
        tr.appendChild(el("td", desc, "pdesc"));
        tb.appendChild(tr);
        if (depth < 3 && p.type === "object" && p.properties) walk(p, depth + 1, `${prefix}${name}.`);
        if (depth < 3 && p.type === "array" && p.items && p.items.properties) walk(p.items, depth + 1, `${prefix}${name}[].`);
      }
    };
    walk(schema, 0, "");
    if (!tb.childNodes.length) {
      const tr = el("tr");
      const td = el("td", "No arguments.", "muted");
      td.colSpan = 4;
      tr.appendChild(td);
      tb.appendChild(tr);
    }
    tbl.appendChild(tb);
    d.appendChild(tbl);
    const raw = el("details", null, "raw");
    raw.appendChild(el("summary", "raw schema"));
    raw.appendChild(el("pre", JSON.stringify(schema, null, 2), "json"));
    d.appendChild(raw);
    return d;
  }

  function drawEdgeInspector(box, e) {
    const es = edgeStatus(e.id);
    const from = nodeById(e.from);
    const to = nodeById(e.to);
    const conn = connectionFor(from && from.kind, to && to.kind);
    const label = (es && es.label) || (conn && conn.label) || "connection";
    inspHead(box, "link", "green", "Connection", label, e.id);
    const state = es ? es.state : "pending";
    const st = el("div", null, `edge-state e-${state}`);
    st.appendChild(el("span", null, "es-dot"));
    st.appendChild(el("strong", { live: "live — Rust compiled this connection", warn: "warning", error: "error — Rust refuses it", pending: "not checked yet" }[state] || state));
    box.appendChild(st);
    const dl = el("dl", null, "kv");
    const row = (k, v, mono) => {
      dl.appendChild(el("dt", k));
      const dd = el("dd", null, mono ? "id" : "");
      if (v instanceof Node) dd.appendChild(v);
      else dd.textContent = v;
      dl.appendChild(dd);
    };
    const nodeLink = (n, id) => {
      const b = el("button", n ? `${cardTitle(n)} (${kindOf(n.kind).label})` : id, "link-btn");
      b.type = "button";
      b.addEventListener("click", () => n && focusOn({ type: "node", id: n.id }));
      return b;
    };
    row("from", nodeLink(from, e.from));
    row("to", nodeLink(to, e.to));
    row("edge", (es && es.edge) || (conn && conn.edge) || "—", true);
    if (es && es.writes) row("writes", es.writes, true);
    else if (conn && conn.writes) row("writes", conn.writes, true);
    box.appendChild(dl);
    if (conn && conn.help) box.appendChild(el("p", conn.help, "insp-help"));
    const issues = (es && es.issues) || [];
    if (issues.length) {
      box.appendChild(el("h3", "Issues"));
      box.appendChild(issueList(issues));
    }
    const del = el("button", "Delete connection", "danger wide");
    del.type = "button";
    del.disabled = !S.editable;
    del.addEventListener("click", deleteSelected);
    box.appendChild(del);
  }

  // ---- preview + finalise --------------------------------------------------

  async function openPreview(tab) {
    if (S.busy) return;
    S.busy = true;
    $("btn-validate").classList.add("busy");
    clearTimeout(S.saveTimer);
    const r = await request("POST", "/api/v1/builder/preview", S.bp);
    S.busy = false;
    $("btn-validate").classList.remove("busy");
    if (r.status !== 200 || !r.body || r.body.toml === undefined) {
      toast(`Preview failed: ${errText(r)}`, "error");
      return;
    }
    S.preview = r.body;
    if (r.body.status) applyStatus(r.body.status);
    S.modalTab = tab || (r.body.load && r.body.load.ok ? "toml" : "checks");
    $("modal").hidden = false;
    requestAnimationFrame(() => $("modal").classList.add("in"));
    drawModal();
  }

  function closeModal() {
    $("modal").classList.remove("in");
    setTimeout(() => {
      $("modal").hidden = true;
    }, 180);
  }

  function drawModal() {
    const p = S.preview;
    if (!p) return;
    const ok = p.load && p.load.ok;
    const v = $("modal-verdict");
    v.className = `pill ${ok ? "p-ok" : "p-error"}`;
    v.textContent = ok ? "loads clean" : `${(p.load && p.load.errors ? p.load.errors.length : 0)} load error${p.load && p.load.errors && p.load.errors.length === 1 ? "" : "s"}`;
    $("modal-title").textContent = `Preview · sandboxes/${S.sandbox}/config.toml`;
    for (const b of document.querySelectorAll("[data-mtab]")) b.setAttribute("aria-selected", String(b.dataset.mtab === S.modalTab));
    const fin = $("modal-finalise");
    const can = Boolean(ok && p.editable);
    fin.disabled = !can;
    fin.classList.toggle("ready", can);
    $("modal-note").textContent = !p.editable ? (p.why_not || "read-only") : ok ? `sha256 ${p.sha256}` : "Fix the load errors first (Checks).";
    const body = clear($("modal-body"));
    switch (S.modalTab) {
      case "toml":
        body.appendChild(tomlView(p.toml || ""));
        break;
      case "diff":
        body.appendChild(diffView(p.diff, p.current));
        break;
      case "checks":
        body.appendChild(checksView(p));
        break;
      case "secrets":
        body.appendChild(secretsView(p.secrets || []));
        break;
    }
  }

  function tomlView(text) {
    const wrap = el("div", null, "code-wrap");
    const bar = el("div", null, "code-bar");
    bar.appendChild(el("span", `${text.split("\n").length} lines`, "muted"));
    bar.appendChild(copyBtn(text, "Copy TOML"));
    wrap.appendChild(bar);
    const pre = el("pre", null, "code toml");
    const lines = text.replace(/\n$/, "").split("\n");
    let inMulti = null;
    lines.forEach((line, i) => {
      const ln = el("span", null, "ln");
      ln.appendChild(el("span", String(i + 1), "ln-n"));
      const body = el("span", null, "ln-b");
      inMulti = tomlLine(line, body, inMulti);
      ln.appendChild(body);
      pre.appendChild(ln);
    });
    wrap.appendChild(pre);
    return wrap;
  }

  // one TOML line into spans; returns the open multi-line string delimiter, if any
  function tomlLine(line, out, inMulti) {
    if (inMulti) {
      const end = line.indexOf(inMulti);
      if (end < 0) {
        out.appendChild(el("span", line, "tk-str"));
        return inMulti;
      }
      out.appendChild(el("span", line.slice(0, end + 3), "tk-str"));
      line = line.slice(end + 3);
      inMulti = null;
      if (line) tomlValue(line, out);
      return null;
    }
    const trimmed = line.trim();
    if (trimmed.startsWith("#")) {
      out.appendChild(el("span", line, "tk-com"));
      return null;
    }
    if (/^\s*\[\[?.*\]\]?\s*(#.*)?$/.test(line)) {
      const m = line.match(/^(\s*)(\[\[?[^\]]*\]\]?)(.*)$/);
      if (m) {
        out.appendChild(document.createTextNode(m[1]));
        out.appendChild(el("span", m[2], "tk-tab"));
        if (m[3]) out.appendChild(el("span", m[3], "tk-com"));
        return null;
      }
    }
    const kv = line.match(/^(\s*)([A-Za-z0-9_\-."']+)(\s*=\s*)(.*)$/);
    if (kv) {
      out.appendChild(document.createTextNode(kv[1]));
      out.appendChild(el("span", kv[2], "tk-key"));
      out.appendChild(el("span", kv[3], "tk-eq"));
      const val = kv[4];
      const mm = val.match(/^("""|''')/);
      if (mm && val.indexOf(mm[1], 3) < 0) {
        out.appendChild(el("span", val, "tk-str"));
        return mm[1];
      }
      tomlValue(val, out);
      return null;
    }
    tomlValue(line, out);
    return null;
  }

  function tomlValue(s, out) {
    const re = /("(?:[^"\\]|\\.)*"|'[^']*'|#.*$|\btrue\b|\bfalse\b|[-+]?\d[\d_]*(?:\.\d+)?(?:[eE][-+]?\d+)?\b|[[\]{},=])/g;
    let last = 0;
    let m;
    while ((m = re.exec(s))) {
      if (m.index > last) out.appendChild(document.createTextNode(s.slice(last, m.index)));
      const t = m[0];
      let cls = "tk-punc";
      if (t[0] === '"' || t[0] === "'") cls = "tk-str";
      else if (t[0] === "#") cls = "tk-com";
      else if (t === "true" || t === "false") cls = "tk-bool";
      else if (/^[-+]?\d/.test(t)) cls = "tk-num";
      out.appendChild(el("span", t, cls));
      last = m.index + t.length;
    }
    if (last < s.length) out.appendChild(document.createTextNode(s.slice(last)));
  }

  function diffView(diff, current) {
    const wrap = el("div", null, "code-wrap");
    const bar = el("div", null, "code-bar");
    if (current === null || current === undefined) bar.appendChild(el("span", "No config.toml on disk yet — everything is new.", "muted"));
    else if (!diff || !diff.trim()) bar.appendChild(el("span", "No change against config.toml on disk.", "muted"));
    else {
      const add = diff.split("\n").filter((l) => l.startsWith("+") && !l.startsWith("+++")).length;
      const del = diff.split("\n").filter((l) => l.startsWith("-") && !l.startsWith("---")).length;
      const s = el("span", null, "diff-stat");
      s.appendChild(el("span", `+${add}`, "d-add"));
      s.appendChild(el("span", ` −${del}`, "d-del"));
      bar.appendChild(s);
    }
    if (diff) bar.appendChild(copyBtn(diff, "Copy diff"));
    wrap.appendChild(bar);
    const pre = el("pre", null, "code diff");
    for (const line of (diff || "").replace(/\n$/, "").split("\n")) {
      if (!diff) break;
      let cls = "d-ctx";
      if (line.startsWith("+++") || line.startsWith("---")) cls = "d-file";
      else if (line.startsWith("+")) cls = "d-add";
      else if (line.startsWith("-")) cls = "d-del";
      else if (line.startsWith("@@")) cls = "d-hunk";
      pre.appendChild(el("span", line || " ", `dl ${cls}`));
    }
    wrap.appendChild(pre);
    return wrap;
  }

  function checksView(p) {
    const box = el("div", null, "checks");
    const load = p.load || { ok: false, errors: [], warnings: [] };
    const head = el("div", null, `check-head ${load.ok ? "ok" : "bad"}`);
    head.appendChild(el("span", null, "check-dot"));
    head.appendChild(el("strong", load.ok ? "The real config loader accepts this config." : "The config loader refuses this config."));
    head.appendChild(el("span", "Rust ran Config::load on a temp copy at sandboxes/<name>/config.toml — the same parse, validation and hardening rules as tengu chat / run.", "muted small"));
    box.appendChild(head);
    if (load.errors && load.errors.length) {
      box.appendChild(el("h3", `Load errors (${load.errors.length})`));
      box.appendChild(issueList(load.errors.map((m) => ({ level: "error", message: m }))));
    }
    if (load.warnings && load.warnings.length) {
      box.appendChild(el("h3", `Load warnings (${load.warnings.length})`));
      box.appendChild(issueList(load.warnings.map((m) => ({ level: "warn", message: m }))));
    }
    const issues = (p.status && p.status.issues) || [];
    box.appendChild(el("h3", `Blueprint issues (${issues.length})`));
    if (!issues.length) box.appendChild(el("p", "None.", "muted"));
    else {
      const ul = el("ul", null, "issues clickable");
      for (const i of issues) {
        const li = el("li", null, `issue i-${i.level === "error" ? "error" : "warn"}`);
        li.appendChild(el("span", i.level === "error" ? "error" : "warning", "i-level"));
        li.appendChild(el("span", i.message, "i-msg"));
        if (i.node || i.edge) {
          li.addEventListener("click", () => {
            closeModal();
            focusOn(i.node ? { type: "node", id: i.node } : { type: "edge", id: i.edge });
          });
        }
        ul.appendChild(li);
      }
      box.appendChild(ul);
    }
    if (!p.editable && p.why_not) {
      box.appendChild(el("h3", "Read-only"));
      box.appendChild(el("p", p.why_not));
    }
    if (p.next && p.next.length) {
      box.appendChild(el("h3", "After Finalise"));
      box.appendChild(cmdList(p.next));
    }
    return box;
  }

  function cmdList(lines) {
    const ul = el("ul", null, "cmds");
    for (const c of lines) {
      const li = el("li");
      li.appendChild(el("code", c));
      li.appendChild(copyBtn(c));
      ul.appendChild(li);
    }
    return ul;
  }

  function secretsView(secrets) {
    const box = el("div", null, "secrets");
    if (!secrets.length) {
      box.appendChild(el("p", "This sandbox names no secret.", "muted"));
      return box;
    }
    box.appendChild(el("p", "Values never pass through this page or the TOML: the config names them, you set them where they live.", "muted small"));
    const tbl = el("table", null, "sec-table");
    const hr = el("tr");
    for (const h of ["env", "backend", "state", "used by", "how"]) hr.appendChild(el("th", h));
    const th = el("thead");
    th.appendChild(hr);
    tbl.appendChild(th);
    const tb = el("tbody");
    for (const s of secrets) {
      const tr = el("tr");
      const env = el("td");
      env.appendChild(el("code", s.env));
      tr.appendChild(env);
      tr.appendChild(el("td", s.backend));
      const st = el("td");
      st.appendChild(el("span", s.state, `chip ch-${{ present: "ok", missing: "error", unknown: "pending", remote: "remote" }[s.state] || "pending"}`));
      tr.appendChild(st);
      tr.appendChild(el("td", (s.used_by || []).join(", ")));
      const how = el("td", null, "how");
      if (s.how) {
        how.appendChild(el("code", s.how));
        how.appendChild(copyBtn(s.how));
        if (s.note) how.appendChild(el("div", `stored in ${s.note}`, "muted"));
      }
      tr.appendChild(how);
      tb.appendChild(tr);
    }
    tbl.appendChild(tb);
    box.appendChild(tbl);
    return box;
  }

  async function finalise() {
    const p = S.preview;
    if (!p || !p.load || !p.load.ok || !p.editable || S.busy) return;
    S.busy = true;
    const btn = $("modal-finalise");
    btn.disabled = true;
    btn.classList.add("busy");
    const r = await request("POST", "/api/v1/builder/finalise", { blueprint: S.bp, sha256: p.sha256 });
    S.busy = false;
    btn.classList.remove("busy");
    if (r.status === 200) {
      closeModal();
      surge();
      toast(`Written ${r.body.written}${r.body.backup ? ` (previous kept as ${r.body.backup})` : ""}`, "ok", { ms: 15000, lines: r.body.next || [] });
      const missing = (r.body.secrets || []).filter((s) => s.state === "missing");
      if (missing.length) toast(`${missing.length} secret${missing.length === 1 ? " is" : "s are"} not set yet — see Secrets.`, "warn", { lines: missing.map((s) => s.how).filter(Boolean) });
      return;
    }
    if (r.status === 409) {
      toast(`Not written: ${errText(r)}. Checked again.`, "warn");
      await reopenPreview();
      return;
    }
    toast(`Not written (${r.status}): ${errText(r)}`, "error", { ms: 9000 });
    if (r.status === 422) await reopenPreview("checks");
    else btn.disabled = false;
  }

  async function reopenPreview(tab) {
    S.busy = false;
    $("modal").hidden = true;
    $("modal").classList.remove("in");
    await openPreview(tab);
  }

  // the "power on" moment after a write: every live edge surges, bolts cross the screen
  function surge() {
    if (REDUCED.matches) return;
    const b = $("burst");
    clear(b);
    const W = window.innerWidth;
    const H = window.innerHeight;
    const svg = sv("svg", { viewBox: `0 0 ${W} ${H}`, class: "bolts" });
    for (let i = 0; i < 7; i++) {
      let x = Math.random() * W;
      let y = -10;
      const pts = [`${x},${y}`];
      while (y < H + 10) {
        x += (Math.random() - 0.5) * 140;
        y += 40 + Math.random() * 70;
        pts.push(`${Math.round(x)},${Math.round(y)}`);
      }
      const pl = sv("polyline", { points: pts.join(" "), class: `bolt b${i % 3}` });
      pl.style.animationDelay = `${i * 70}ms`;
      svg.appendChild(pl);
    }
    b.appendChild(svg);
    b.classList.remove("on");
    void b.offsetWidth;
    b.classList.add("on");
    setTimeout(() => {
      b.classList.remove("on");
      clear(b);
    }, 1500);
    $("edge-layer").classList.add("surge-all");
    setTimeout(() => $("edge-layer").classList.remove("surge-all"), 1600);
    let i = 0;
    for (const e of S.bp.edges) if (S.edgeState.get(e.id) === "live") setTimeout(() => spark(e.id), 60 * i++);
    for (const ne of S.nodeEls.values()) {
      ne.card.classList.add("charged");
      setTimeout(() => ne.card.classList.remove("charged"), 1400);
    }
  }

  // ---- debug drawer --------------------------------------------------------

  function debugText() {
    switch (S.debugTab) {
      case "blueprint":
        return JSON.stringify(S.bp, null, 2);
      case "response":
        return JSON.stringify(S.lastResponse, null, 2);
      case "log":
        return S.log.map((l) => `${l.at}  ${l.method.padEnd(4)} ${l.path}  ${l.status}  ${l.ms}ms`).join("\n");
      default:
        return JSON.stringify((S.status && S.status.issues) || [], null, 2);
    }
  }

  function drawDebug() {
    for (const b of document.querySelectorAll("[data-tab]")) b.setAttribute("aria-selected", String(b.dataset.tab === S.debugTab));
    const body = clear($("debug-body"));
    if (S.debugTab === "issues") {
      const issues = (S.status && S.status.issues) || [];
      if (!issues.length) body.appendChild(el("p", S.status ? "No issues." : "No status yet.", "muted"));
      else {
        const tbl = el("table", null, "dbg-table");
        const hr = el("tr");
        for (const h of ["level", "where", "field", "message"]) hr.appendChild(el("th", h));
        tbl.appendChild(hr);
        for (const i of issues) {
          const tr = el("tr", null, `i-${i.level === "error" ? "error" : "warn"}`);
          tr.appendChild(el("td", i.level));
          tr.appendChild(el("td", i.node || i.edge || "sandbox", "id"));
          tr.appendChild(el("td", i.field || "", "id"));
          tr.appendChild(el("td", i.message));
          if (i.node || i.edge) tr.addEventListener("click", () => focusOn(i.node ? { type: "node", id: i.node } : { type: "edge", id: i.edge }));
          tbl.appendChild(tr);
        }
        body.appendChild(tbl);
      }
      return;
    }
    if (S.debugTab === "log") {
      const tbl = el("table", null, "dbg-table");
      const hr = el("tr");
      for (const h of ["at", "method", "path", "status", "ms"]) hr.appendChild(el("th", h));
      tbl.appendChild(hr);
      for (const l of S.log) {
        const tr = el("tr", null, l.status >= 400 || l.status === 0 ? "i-error" : "");
        tr.appendChild(el("td", l.at, "id"));
        tr.appendChild(el("td", l.method));
        tr.appendChild(el("td", l.path, "id"));
        tr.appendChild(el("td", String(l.status)));
        tr.appendChild(el("td", String(l.ms)));
        tbl.appendChild(tr);
      }
      body.appendChild(tbl);
      return;
    }
    body.appendChild(el("pre", debugText(), "json"));
  }

  function toggleDebug(force) {
    const d = $("debug");
    const open = force === undefined ? d.hidden : force;
    d.hidden = !open;
    $("btn-debug").setAttribute("aria-pressed", String(open));
    document.body.classList.toggle("debug-open", open);
    if (open) drawDebug();
    requestAnimationFrame(drawMinimap);
  }

  // ---- theme ---------------------------------------------------------------

  function applyTheme(t) {
    if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
    else delete document.documentElement.dataset.theme;
  }

  function toggleTheme() {
    const cur = document.documentElement.dataset.theme || (window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
    const next = cur === "dark" ? "light" : "dark";
    applyTheme(next);
    try {
      localStorage.setItem("builder-theme", next);
    } catch (_) { /* per-viewer convenience only */ }
  }

  // ---- wiring --------------------------------------------------------------

  function wire() {
    wireStage();
    $("pal-search").addEventListener("input", drawPalette);
    $("btn-validate").addEventListener("click", () => openPreview("checks"));
    $("btn-finalise").addEventListener("click", () => openPreview("toml"));
    $("btn-debug").addEventListener("click", () => toggleDebug());
    $("debug-close").addEventListener("click", () => toggleDebug(false));
    $("debug-copy").addEventListener("click", () => copyText(debugText(), $("debug-copy")));
    $("btn-theme").addEventListener("click", toggleTheme);
    $("btn-undo").addEventListener("click", () => restore(S.undo, S.redo));
    $("btn-redo").addEventListener("click", () => restore(S.redo, S.undo));
    $("status-pill").addEventListener("click", () => {
      select(null);
      if ((S.status && S.status.issues && S.status.issues.length) || false) {
        S.debugTab = "issues";
        toggleDebug(true);
      }
    });
    $("btn-palette").addEventListener("click", () => {
      const p = $("palette");
      const open = !p.classList.contains("open");
      p.classList.toggle("open", open);
      $("btn-palette").setAttribute("aria-expanded", String(open));
    });
    for (const b of document.querySelectorAll("[data-tab]")) {
      b.addEventListener("click", () => {
        S.debugTab = b.dataset.tab;
        drawDebug();
      });
    }
    for (const b of document.querySelectorAll("[data-mtab]")) {
      b.addEventListener("click", () => {
        S.modalTab = b.dataset.mtab;
        drawModal();
      });
    }
    $("modal-close").addEventListener("click", closeModal);
    $("modal").addEventListener("pointerdown", (e) => {
      if (e.target === $("modal")) closeModal();
    });
    $("modal-recheck").addEventListener("click", () => reopenPreview(S.modalTab));
    $("modal-finalise").addEventListener("click", finalise);
    window.addEventListener("keydown", onKey);
    window.addEventListener("resize", () => drawMinimap());
    REDUCED.addEventListener("change", () => S.bp && drawAll());
  }

  function onKey(e) {
    const mod = e.metaKey || e.ctrlKey;
    if (!$("modal").hidden) {
      if (e.key === "Escape") closeModal();
      return;
    }
    if (mod && e.key.toLowerCase() === "s") {
      e.preventDefault();
      save();
      return;
    }
    if (mod && e.key === "Enter") {
      e.preventDefault();
      openPreview("checks");
      return;
    }
    if (typing(e)) return;
    if (mod && e.key.toLowerCase() === "z") {
      e.preventDefault();
      if (e.shiftKey) restore(S.redo, S.undo);
      else restore(S.undo, S.redo);
      return;
    }
    if (mod && e.key.toLowerCase() === "y") {
      e.preventDefault();
      restore(S.redo, S.undo);
      return;
    }
    if (e.key === "Delete" || e.key === "Backspace") {
      if (S.sel) {
        e.preventDefault();
        deleteSelected();
      }
      return;
    }
    if (e.key === "Escape") {
      select(null);
      $("palette").classList.remove("open");
      return;
    }
    if (!mod && (e.key === "f" || e.key === "F")) {
      e.preventDefault();
      fit();
    }
  }

  // ---- start ---------------------------------------------------------------

  async function start() {
    try {
      applyTheme(localStorage.getItem("builder-theme"));
    } catch (_) { /* default theme */ }
    const h = parseHash();
    S.token = readToken(h);
    writeHash();
    if (!S.token) {
      banner("No Studio token: open the exact URL `tengu studio` (or `tengu sandbox new`) printed — it ends in #t=…, then go to /builder.", "error");
      setSaveInd("error", "no token");
      return;
    }
    $("link-studio").href = `/#t=${encodeURIComponent(S.token)}`;
    wire();
    const r = await request("GET", "/api/v1/builder");
    if (r.status !== 200 || !r.body || !r.body.palette) {
      banner(`Builder unavailable: ${errText(r)}${r.status === 404 ? " — start Studio with --allow-edit." : ""}`, "error");
      setSaveInd("error", errText(r));
      return;
    }
    const d = r.body;
    S.sandbox = d.sandbox;
    S.editable = Boolean(d.editable);
    S.whyNot = d.why_not || null;
    S.palette = d.palette;
    S.kinds = new Map((d.palette.kinds || []).map((k) => [k.kind, k]));
    S.bp = d.blueprint || { schema_version: 1, sandbox: d.sandbox, nodes: [], edges: [] };
    if (!Array.isArray(S.bp.nodes)) S.bp.nodes = [];
    if (!Array.isArray(S.bp.edges)) S.bp.edges = [];
    $("sandbox-name").textContent = S.sandbox;
    document.title = `Tengu Builder · ${S.sandbox}`;
    document.body.classList.toggle("readonly", !S.editable);
    if (!S.editable) {
      banner(`Read-only: ${S.whyNot || "this sandbox cannot be edited here"}`, "warn");
      setSaveInd("readonly", S.whyNot || "");
    } else setSaveInd("saved");
    drawPalette();
    drawAll();
    // Rust always sends a view; the untouched default (0, 0, 100 %) means
    // "never placed" — fit the cards instead.
    const v0 = d.blueprint && d.blueprint.view;
    const hadView = Boolean(v0 && !(v0.x === 0 && v0.y === 0 && v0.zoom === 1));
    requestAnimationFrame(() => {
      if (hadView) applyView();
      else fit();
    });
    drawUndo();
    if (d.status) applyStatus(d.status);
    else drawInspector();
  }

  start();
})();
