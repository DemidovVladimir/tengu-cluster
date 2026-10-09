// Tengu Studio (ST-21 graph, inspector, timeline; ST-22 runs and replay;
// ST-30 controls). Draws what /api/v1 serves and decides nothing: the graph and its
// columns (`layer` / `order`), each node's tone, the highlighted edges, the
// grey legal set and the header facts (the board, folded in Rust from the
// trace), run states, the health verdict and the validated config all
// arrive computed; so do the control state and which button may act (GET
// /api/v1/control — the controls are hidden unless it says control is on).
// Here: pixels, a list filter, a POST per button, and keeping the place on
// reload (the URL fragment). No animation: the screen changes when an
// event arrives.
"use strict";

(() => {
  const PAGE = 1000; // /events page size (the API's largest)
  const POLL_MS = 5000; // runs + health
  const MAX_ROWS = 2000; // timeline rows drawn (the newest; filter for more)
  const LINE_CHARS = 24; // a node label wraps here — every character stays
  const CHAR_PX = 7;
  const PAD_X = 12;
  const GAP_X = 40;
  const GAP_Y = 14;
  const LINE_PX = 15;
  const MIN_W = 100;
  const FIT_MIN = 0.7; // shrink a graph to its panel down to this scale, else scroll
  const FACETS = ["loop", "agent", "model", "tool"];

  const $ = (id) => document.getElementById(id);
  const SVG_NS = $("graph").namespaceURI;
  const enc = encodeURIComponent;

  const S = {
    token: null,
    tokenInHash: false,
    meta: null,
    graph: null,
    graphKey: null,
    layout: null,
    nodeEls: null,
    edgeEls: null,
    mode: "live",
    runs: [],
    liveRunId: null,
    holder: null,
    waiting: null,
    runId: null,
    run: null,
    runGraph: null,
    events: [],
    bySeq: new Map(),
    lastSeq: 0,
    upto: null,
    board: null,
    sel: { node: null, event: null },
    inspect: null,
    nodeDetail: null,
    live: null,
    follow: null,
    syncing: false,
    again: false,
    seekTimer: null,
    control: null,
    ctlBusy: false,
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

  function chip(text, tone) {
    return el("span", text, `badge t-${tone || "none"}`);
  }

  function pre(value) {
    return el("pre", JSON.stringify(value, null, 2));
  }

  function linkish(text, onClick) {
    const b = el("button", text, "linkish");
    b.type = "button";
    b.addEventListener("click", onClick);
    return b;
  }

  function isoTime(ms) {
    return ms ? new Date(ms).toISOString().replace("T", " ").replace("Z", " UTC") : "—";
  }

  function clock(ms) {
    return ms ? new Date(ms).toISOString().slice(11, 23) : "—";
  }

  function notice(text) {
    const n = $("notice");
    n.textContent = text || "";
    n.hidden = !text;
  }

  function kv(pairs) {
    const dl = el("dl", null, "kv");
    for (const [k, v] of pairs) {
      if (v === undefined || v === null || v === "") continue;
      const dd = el("dd");
      if (v instanceof Node) dd.append(v);
      else dd.textContent = String(v);
      dl.append(el("dt", k), dd);
    }
    return dl;
  }

  // ---- token + place (URL fragment) ----------------------------------------

  function parseHash() {
    const out = {};
    for (const part of location.hash.replace(/^#/, "").split("&")) {
      const i = part.indexOf("=");
      if (i > 0) out[part.slice(0, i)] = decodeURIComponent(part.slice(i + 1));
    }
    return out;
  }

  // The token rides in the fragment (`#t=<hex>`), which a browser never
  // sends; kept for this tab, then dropped from the address bar.
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
    const p = [];
    if (S.tokenInHash) p.push(["t", S.token]);
    p.push(["mode", S.mode]);
    if (S.mode === "replay" && S.runId) p.push(["run", S.runId]);
    if (S.mode === "replay" && S.upto !== null) p.push(["at", String(S.upto)]);
    if (S.sel.node) p.push(["node", S.sel.node]);
    if (S.sel.event) p.push(["ev", S.sel.event]);
    if (S.inspect) p.push(["show", S.inspect]);
    const h = "#" + p.map(([k, v]) => `${k}=${enc(v)}`).join("&");
    if (location.hash !== h) history.replaceState(null, "", h);
  }

  async function api(path) {
    const r = await fetch(path, { headers: { "X-Studio-Token": S.token }, cache: "no-store" });
    const body = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(`${path}: ${r.status} ${body.error || r.statusText}`);
    return body;
  }

  // A change request: the token header + JSON; the browser adds `Origin` and
  // `Sec-Fetch-Site: same-origin` itself (the server requires all three).
  // Answers with its verdict even when refused (403 / 409 / 422 / 429 / 500).
  async function post(path, body) {
    const r = await fetch(path, {
      method: "POST",
      headers: { "X-Studio-Token": S.token, "Content-Type": "application/json" },
      body: JSON.stringify(body || {}),
      cache: "no-store",
    });
    const out = await r.json().catch(() => ({}));
    return { status: r.status, body: out };
  }

  // ---- meta, legend, health, runs ------------------------------------------

  async function loadMeta() {
    const m = await api("/api/v1/meta");
    S.meta = m;
    document.title = `Tengu Studio · ${m.sandbox}`;
    $("f-sandbox").textContent = m.sandbox;
    $("f-config").textContent = m.config_hash || "none (no config file)";
    const legend = $("legend");
    legend.replaceChildren();
    for (const t of m.tones.tones) {
      const li = el("li");
      li.append(chip(t.tone, t.tone), el("span", t.meaning));
      legend.append(li);
    }
    const status = $("legend-status");
    status.replaceChildren();
    for (const s of m.tones.statuses) {
      const li = el("li");
      li.append(chip(s.status, s.tone), el("span", s.meaning, "muted"));
      status.append(li);
    }
    const sel = document.querySelector('#filters select[name="status"]');
    sel.replaceChildren(new Option("any", ""));
    for (const s of m.tones.statuses) sel.append(new Option(s.status, s.status));
  }

  async function loadHealth() {
    const h = await api("/api/v1/health");
    $("f-health").replaceChildren(chip(h.live ? "live" : "not live", h.tone));
    const list = $("checks");
    list.replaceChildren();
    for (const c of h.checks) {
      const li = el("li");
      li.append(chip(c.ok ? "ok" : "FAIL", c.tone), el("span", c.subject), el("span", c.detail, "detail"));
      list.append(li);
    }
  }

  async function loadRuns() {
    const r = await api("/api/v1/runs");
    S.runs = r.runs;
    S.liveRunId = r.live_run_id;
    S.holder = r.holder;
    renderRuns();
  }

  function renderRuns() {
    const body = $("runs");
    body.replaceChildren();
    for (const run of S.runs.slice().reverse()) {
      const tr = el("tr", null, run.run_id === S.runId ? "sel" : "");
      const state = el("td");
      state.append(chip(run.state));
      const cfg = el("td");
      cfg.append(run.config_current ? chip("current") : chip("config changed", "amber"));
      tr.append(
        el("td", run.run_id),
        el("td", run.kind || "?"),
        state,
        cfg,
        el("td", run.runtime_id || "—"),
        el("td", isoTime(run.started_ms)),
        el("td", run.events),
        el("td", `${run.last_kind} (${run.last_status})`),
      );
      tr.tabIndex = 0;
      tr.addEventListener("click", () => openReplay(run.run_id, null));
      tr.addEventListener("keydown", (e) => {
        if (e.key === "Enter") openReplay(run.run_id, null);
      });
      body.append(tr);
    }
  }

  // ---- graph: layout from Rust's layer/order, colours from the board -------

  function wrap(text) {
    const s = String(text);
    const out = [];
    let i = 0;
    while (s.length - i > LINE_CHARS) {
      let cut = i + LINE_CHARS;
      for (let j = cut; j > i + LINE_CHARS - 8; j--) {
        if ("/:-_ .".includes(s[j - 1])) {
          cut = j;
          break;
        }
      }
      out.push(s.slice(i, cut));
      i = cut;
    }
    out.push(s.slice(i));
    return out;
  }

  function subLabel(n) {
    return n.attrs && n.attrs.effect ? `${n.kind} · ${n.attrs.effect}` : n.kind;
  }

  function layout(g) {
    const cols = new Map();
    for (const n of g.nodes) {
      if (!cols.has(n.layer)) cols.set(n.layer, []);
      cols.get(n.layer).push(n);
    }
    const pos = new Map();
    let x = GAP_X / 2;
    let height = 0;
    for (const layer of [...cols.keys()].sort((a, b) => a - b)) {
      const boxes = cols
        .get(layer)
        .slice()
        .sort((a, b) => a.order - b.order || (a.id < b.id ? -1 : 1))
        .map((n) => ({ n, lines: wrap(n.label), sub: subLabel(n) }));
      const chars = Math.max(...boxes.map((b) => Math.max(b.sub.length, ...b.lines.map((l) => l.length))));
      const w = Math.max(MIN_W, chars * CHAR_PX + 2 * PAD_X);
      let y = GAP_Y;
      for (const b of boxes) {
        const h = 16 + b.lines.length * LINE_PX + 14;
        pos.set(b.n.id, { x, y, w, h, lines: b.lines, sub: b.sub, node: b.n });
        y += h + GAP_Y;
      }
      height = Math.max(height, y);
      x += w + GAP_X;
    }
    return { pos, width: Math.max(x - GAP_X / 2, 1), height: Math.max(height, 1) };
  }

  function edgePath(a, b) {
    const y1 = a.y + a.h / 2;
    const y2 = b.y + b.h / 2;
    if (a.x < b.x) {
      const x1 = a.x + a.w;
      const x2 = b.x - 2;
      const dx = Math.max(24, (x2 - x1) / 2);
      return `M${x1},${y1} C${x1 + dx},${y1} ${x2 - dx},${y2} ${x2},${y2}`;
    }
    if (a.x > b.x) {
      const x1 = a.x;
      const x2 = b.x + b.w + 2;
      const dx = Math.max(24, (x1 - x2) / 2);
      return `M${x1},${y1} C${x1 - dx},${y1} ${x2 + dx},${y2} ${x2},${y2}`;
    }
    const xr = a.x + a.w;
    const bulge = 28 + Math.abs(y2 - y1) / 6;
    return `M${xr},${y1} C${xr + bulge},${y1} ${xr + bulge},${y2} ${xr + 2},${y2}`;
  }

  const edgeKey = (e) => `${e.from}\n${e.to}\n${e.kind}`;

  function drawGraph() {
    const g = S.graph;
    const svg = $("graph");
    const L = layout(g);
    S.layout = L;
    svg.replaceChildren();
    svg.setAttribute("viewBox", `0 0 ${L.width} ${L.height}`);
    svg.setAttribute("width", L.width);
    svg.setAttribute("height", L.height);
    const defs = sv("defs");
    for (const t of ["none", "green", "amber", "red", "plain"]) {
      const m = sv("marker", {
        id: `arrow-${t}`,
        viewBox: "0 0 8 8",
        refX: 7,
        refY: 4,
        markerWidth: 7,
        markerHeight: 7,
        orient: "auto-start-reverse",
      });
      m.append(sv("path", { d: "M0,0 L8,4 L0,8 z", class: t === "none" ? "arrow" : `arrow t-${t}` }));
      defs.append(m);
    }
    svg.append(defs);
    const edges = sv("g");
    S.edgeEls = new Map();
    for (const e of g.edges) {
      const a = L.pos.get(e.from);
      const b = L.pos.get(e.to);
      if (!a || !b) continue;
      const p = sv("path", { d: edgePath(a, b), class: "e", "marker-end": "url(#arrow-none)" });
      const t = sv("title");
      t.textContent = `${e.from} —${e.kind}→ ${e.to}`;
      p.append(t);
      edges.append(p);
      S.edgeEls.set(edgeKey(e), p);
    }
    const nodes = sv("g");
    S.nodeEls = new Map();
    for (const [id, b] of L.pos) {
      const g1 = sv("g", {
        class: "n",
        transform: `translate(${b.x},${b.y})`,
        tabindex: 0,
        role: "button",
        "aria-label": `${b.node.kind} ${id}`,
      });
      g1.append(sv("rect", { width: b.w, height: b.h, rx: 7 }));
      b.lines.forEach((line, i) => {
        const t = sv("text", { x: PAD_X, y: 18 + i * LINE_PX });
        t.textContent = line;
        g1.append(t);
      });
      const sub = sv("text", { x: PAD_X, y: 20 + b.lines.length * LINE_PX, class: "sub" });
      sub.textContent = b.sub;
      g1.append(sub);
      const title = sv("title");
      title.textContent = id;
      g1.append(title);
      g1.addEventListener("click", () => selectNode(id));
      g1.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          selectNode(id);
        }
      });
      nodes.append(g1);
      S.nodeEls.set(id, g1);
    }
    svg.append(edges, nodes);
    $("graph-meta").textContent =
      `${g.nodes.length} nodes · ${g.edges.length} edges` + (g.map ? ` · execution map ${g.map.sha256}` : "");
    paintGraph();
    fitGraph();
  }

  // Pixels only: a graph a little wider than its panel is scaled to fit;
  // a much wider one keeps its size and scrolls.
  function fitGraph() {
    if (!S.layout) return;
    const svg = $("graph");
    const room = svg.parentElement.clientWidth;
    const scale = room / S.layout.width;
    svg.classList.toggle("fit", scale < 1 && scale >= FIT_MIN);
  }

  // The board says each node's tone (+ why: an event, the step's legal set, a
  // map's narrowed_out) and which edges an action / tool event highlighted;
  // without a board (no run yet) nothing is coloured.
  function paintGraph() {
    if (!S.nodeEls) return;
    const draws = new Map((S.board ? S.board.nodes : []).map((d) => [d.node_id, d]));
    for (const [id, g1] of S.nodeEls) {
      const d = draws.get(id);
      const tone = d ? d.tone : null;
      const cls = ["n", tone ? `t-${tone}` : "", d && d.latest ? "latest" : "", S.sel.node === id ? "sel" : ""];
      g1.setAttribute("class", cls.filter(Boolean).join(" "));
      const title = g1.querySelector("title");
      const why = d && d.why ? ` · ${d.why}` : "";
      const mark = d && d.mark ? ` · ${d.mark.event_kind} (${d.mark.status}) ${d.mark.event_id}` : "";
      title.textContent = `${id}${tone ? ` · ${tone}` : ""}${why}${mark}`;
    }
    const hl = new Map((S.board ? S.board.edges : []).map((e) => [edgeKey(e), e]));
    for (const [k, p] of S.edgeEls) {
      const e = hl.get(k);
      p.setAttribute("class", e ? `e hl t-${e.tone}${e.latest ? " latest" : ""}` : "e");
      p.setAttribute("marker-end", `url(#arrow-${e ? e.tone : "none"})`);
    }
  }

  async function ensureGraph(info) {
    const key = info && info.map && !info.note ? info.map : "";
    if (S.graph && S.graphKey === key) return;
    S.graph = await api(key ? `/api/v1/graph?map=${enc(key)}` : "/api/v1/graph");
    S.graphKey = key;
    drawGraph();
    // The selected node's config is the drawn graph's (a map narrows it).
    if (S.sel.node && S.inspect === "node") selectNode(S.sel.node);
  }

  // ---- runs: load, follow, replay ------------------------------------------

  function clearRun() {
    S.runId = null;
    S.events = [];
    S.bySeq = new Map();
    S.lastSeq = 0;
    S.board = null;
    S.run = null;
    S.runGraph = null;
    S.upto = null;
    if (S.inspect === "event") {
      S.sel.event = null;
      S.inspect = S.sel.node ? "node" : null;
    }
  }

  function setRun(id, upto) {
    clearRun();
    S.runId = id;
    S.upto = upto;
    writeHash();
    renderAll();
    return sync();
  }

  // Coalesced: one fetch loop at a time; an event that arrives meanwhile
  // asks for one more pass. Events come from the file (`/events`), so a
  // live view and a replay of the same run are the same list.
  async function sync() {
    if (S.syncing) {
      S.again = true;
      return;
    }
    S.syncing = true;
    try {
      do {
        S.again = false;
        if (S.runId) await syncOnce(S.runId);
      } while (S.again);
      notice("");
    } catch (e) {
      notice(String(e.message || e));
    } finally {
      S.syncing = false;
    }
  }

  async function syncOnce(id) {
    let more = true;
    while (more) {
      const p = await api(`/api/v1/runs/${enc(id)}/events?after=${S.lastSeq}&limit=${PAGE}`);
      if (id !== S.runId) {
        S.again = true;
        return;
      }
      S.runGraph = p.graph;
      for (const ev of p.events) {
        if (!S.bySeq.has(ev.seq)) {
          S.events.push(ev);
          S.bySeq.set(ev.seq, ev);
        }
      }
      S.lastSeq = p.next_after;
      more = p.more;
    }
    await loadBoard(id);
    if (id !== S.runId) {
      S.again = true;
      return;
    }
    renderAll();
  }

  async function loadBoard(id) {
    const at = S.mode === "replay" && S.upto !== null ? `?upto=${S.upto}` : "";
    const b = await api(`/api/v1/runs/${enc(id)}/board${at}`);
    if (id !== S.runId) return;
    S.board = b.board;
    S.run = b.run;
    S.runGraph = b.graph;
    await ensureGraph(b.graph);
  }

  function stopSources() {
    for (const k of ["live", "follow"]) {
      if (S[k]) S[k].close();
      S[k] = null;
    }
  }

  // Live = the run of the runtime that holds the lease, whatever was on
  // screen before; until the stream names one, no run is shown.
  function goLive() {
    stopSources();
    S.mode = "live";
    clearRun();
    S.waiting = "connecting to the live stream…";
    writeHash();
    renderAll();
    ensureGraph(null).catch((e) => notice(String(e.message || e)));
    const src = new EventSource(`/api/v1/live/stream?token=${enc(S.token)}`);
    S.live = src;
    src.addEventListener("run", (e) => {
      const a = JSON.parse(e.data);
      if (a.run_id) {
        S.waiting = null;
        if (a.run_id !== S.runId) setRun(a.run_id, null);
        else renderNote();
      } else {
        S.waiting = `${a.reason}: ${a.detail}`;
        renderNote();
      }
    });
    src.addEventListener("trace", (e) => {
      const ev = JSON.parse(e.data);
      if (ev.run_id === S.runId) sync();
    });
    // After `lagged` the server closes; the browser reconnects with the
    // last event id and the rest is read from the file by `seq` anyway.
    src.addEventListener("lagged", () => sync());
    src.onerror = () => {
      if (src.readyState === EventSource.CLOSED) notice("Live stream closed by the server — reload the page.");
    };
  }

  async function openReplay(runId, upto) {
    stopSources();
    S.mode = "replay";
    S.waiting = null;
    await setRun(runId, upto);
    // A run still being written: its stream rings when a line lands.
    if (S.runId === runId && S.run && S.run.state !== "closed") {
      const src = new EventSource(`/api/v1/runs/${enc(runId)}/stream?after=${S.lastSeq}&token=${enc(S.token)}`);
      S.follow = src;
      src.addEventListener("trace", () => sync());
      src.addEventListener("lagged", () => sync());
    }
  }

  function seek(seq) {
    if (S.mode !== "replay" || !S.runId) return;
    S.upto = seq >= S.lastSeq ? null : Math.max(0, seq);
    writeHash();
    renderReplayBar();
    clearTimeout(S.seekTimer);
    S.seekTimer = setTimeout(async () => {
      try {
        await loadBoard(S.runId);
        renderAll();
      } catch (e) {
        notice(String(e.message || e));
      }
    }, 60);
  }

  // ---- render --------------------------------------------------------------

  function renderAll() {
    renderHeader();
    renderNote();
    paintGraph();
    renderCounters();
    renderReplayBar();
    renderTimeline();
    renderInspector();
    renderEvidence();
    renderRuns();
  }

  function renderHeader() {
    $("mode-badge").textContent = S.mode === "live" ? "Live" : "Replay";
    $("btn-live").setAttribute("aria-pressed", String(S.mode === "live"));
    $("btn-replay").setAttribute("aria-pressed", String(S.mode === "replay"));
    const h = S.board ? S.board.header : null;
    $("f-run").textContent = S.runId || "—";
    $("f-runtime-id").textContent = (h && h.runtime_id) || "—";
    const rt = h && h.runtime;
    const runtime = $("f-runtime");
    if (rt) runtime.replaceChildren(chip(rt.state, rt.tone));
    else runtime.textContent = h && h.kind === "decide" ? "none (tengu decide)" : "—";
    $("f-model").textContent = h && h.model ? h.model.value : "—";
    const rb = $("run-badge");
    rb.hidden = !S.run;
    if (S.run) {
      rb.textContent = `run ${S.run.state}`;
      rb.className = "badge t-none";
    }
    $("config-badge").hidden = !S.run || S.run.config_current;
  }

  function renderNote() {
    const notes = [];
    if (S.mode === "live" && S.waiting) notes.push(S.waiting);
    if (S.run && !S.run.config_current) {
      const was = S.run.summary.config_hash || "none";
      notes.push(
        `This run was recorded with config ${was}; it is drawn on the current config ${S.meta.config_hash || "none"} — its own graph is not rebuilt.`,
      );
    }
    if (S.runGraph && S.runGraph.note) notes.push(S.runGraph.note);
    if (S.board && S.board.not_in_graph.length)
      notes.push(`Events name nodes this graph lacks (not drawn): ${S.board.not_in_graph.join(", ")}`);
    const n = $("run-note");
    n.textContent = notes.join(" · ");
    n.hidden = notes.length === 0;
  }

  function renderCounters() {
    const box = $("counters");
    box.replaceChildren();
    const loops = S.board ? S.board.header.loops : {};
    for (const [name, seen] of Object.entries(loops)) {
      const span = el("span", null, "loop");
      span.append(el("b", `loop ${name}`), document.createTextNode(" "));
      const parts = [];
      for (const [k, v] of Object.entries(seen.value || {})) {
        if (v !== null && typeof v !== "object") parts.push(`${k} ${v}`);
      }
      span.append(el("span", parts.join(" · "), "kv"));
      box.append(span);
    }
  }

  function renderReplayBar() {
    const bar = $("replay-bar");
    bar.hidden = S.mode !== "replay" || !S.runId;
    if (bar.hidden) return;
    const r = $("r-seq");
    r.max = String(S.lastSeq);
    r.value = String(S.upto === null ? S.lastSeq : S.upto);
    const at = S.upto === null ? S.lastSeq : S.upto;
    const open = S.run && S.run.state !== "closed" ? " · run still open" : "";
    $("r-label").textContent = `seq ${at} of ${S.lastSeq}${open}`;
  }

  function filters() {
    const f = {};
    for (const [k, v] of new FormData($("filters")).entries()) if (v) f[k] = String(v);
    return f;
  }

  function matches(ev, f) {
    const v = ev.view || {};
    if (f.session && !(ev.session_id || "").includes(f.session)) return false;
    for (const k of FACETS) if (f[k] && v[k] !== f[k]) return false;
    if (f.status && ev.status !== f.status) return false;
    if (f.call_id && !(ev.call_id || "").includes(f.call_id)) return false;
    return true;
  }

  function fillFacets() {
    const sets = Object.fromEntries(FACETS.map((k) => [k, new Set()]));
    const sessions = new Set();
    const calls = new Set();
    for (const ev of S.events) {
      const v = ev.view || {};
      for (const k of FACETS) if (v[k]) sets[k].add(v[k]);
      if (ev.session_id) sessions.add(ev.session_id);
      if (ev.call_id) calls.add(ev.call_id);
    }
    for (const k of FACETS) {
      const sel = document.querySelector(`#filters select[name="${k}"]`);
      const keep = sel.value;
      sel.replaceChildren(new Option("any", ""));
      for (const v of [...sets[k]].sort()) sel.append(new Option(v, v));
      if (keep && !sets[k].has(keep)) sel.append(new Option(keep, keep));
      sel.value = keep;
    }
    for (const [id, set] of [["dl-session", sessions], ["dl-call", calls]]) {
      const dl = $(id);
      dl.replaceChildren(...[...set].sort().map((v) => new Option(v)));
    }
  }

  function renderTimeline() {
    fillFacets();
    const f = filters();
    const rows = S.events.filter((e) => matches(e, f));
    const shown = rows.slice(-MAX_ROWS);
    const more = shown.length < rows.length ? ` · drawing the newest ${MAX_ROWS}; filter for the rest` : "";
    $("tl-count").textContent = `${rows.length} of ${S.events.length} events${more}`;
    const scroller = $("tl-scroll");
    const atBottom = scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 4;
    const body = $("tl-body");
    body.replaceChildren();
    const at = S.mode === "replay" && S.upto !== null ? S.upto : null;
    for (const ev of shown) {
      const cls = [];
      if (S.sel.event === ev.event_id) cls.push("sel");
      if (at !== null && ev.seq > at) cls.push("future");
      if (at !== null && ev.seq === at) cls.push("at");
      const tr = el("tr", null, cls.join(" "));
      const status = el("td");
      status.append(chip(ev.status, ev.view ? ev.view.tone : null));
      status.className = "nw";
      tr.append(
        el("td", ev.seq, "nw"),
        el("td", clock(ev.ts_ms), "nw"),
        el("td", ev.kind, "nw"),
        status,
        el("td", ev.node_id || "—"),
        el("td", ev.session_id || "—"),
        el("td", ev.call_id || "—"),
        el("td", ev.duration_ms === null || ev.duration_ms === undefined ? "" : ev.duration_ms, "nw"),
      );
      tr.tabIndex = 0;
      tr.addEventListener("click", () => selectEvent(ev.event_id));
      tr.addEventListener("keydown", (e) => {
        if (e.key === "Enter") selectEvent(ev.event_id);
      });
      body.append(tr);
    }
    if (S.mode === "live" && atBottom) scroller.scrollTop = scroller.scrollHeight;
  }

  // ---- inspector -----------------------------------------------------------

  function findEvent(id) {
    return S.events.find((e) => e.event_id === id) || null;
  }

  function selectEvent(id) {
    S.sel.event = id;
    S.inspect = "event";
    const ev = findEvent(id);
    if (ev && S.mode === "replay") seek(ev.seq);
    writeHash();
    renderTimeline();
    renderInspector();
    renderEvidence();
  }

  async function selectNode(id) {
    S.sel.node = id;
    S.inspect = "node";
    writeHash();
    paintGraph();
    try {
      const q = S.graphKey ? `?map=${enc(S.graphKey)}` : "";
      S.nodeDetail = await api(`/api/v1/nodes/${enc(id)}${q}`);
    } catch (e) {
      S.nodeDetail = { error: String(e.message || e), node: { id } };
    }
    renderInspector();
    renderEvidence();
  }

  function renderInspector() {
    const box = $("inspector");
    box.replaceChildren();
    if (S.inspect === "event" && S.sel.event) {
      const ev = findEvent(S.sel.event);
      if (ev) return eventDetail(box, ev);
    }
    if (S.inspect === "node" && S.nodeDetail && S.nodeDetail.node.id === S.sel.node) {
      return nodeDetail(box, S.nodeDetail);
    }
    box.append(el("p", "Select a node in the graph or an event in the timeline.", "muted"));
  }

  function eventLink(e) {
    return linkish(`#${e.seq} ${e.kind} (${e.status})`, () => selectEvent(e.event_id));
  }

  function nodeDetail(box, d) {
    if (d.error) {
      box.append(el("p", d.error, "notice"));
      return;
    }
    const n = d.node;
    const title = el("div", null, "title");
    title.append(el("h3", n.label), chip(n.kind));
    box.append(title, el("p", n.id, "id"));
    const draw = S.board ? S.board.nodes.find((x) => x.node_id === n.id) : null;
    const rows = [];
    if (draw && draw.tone) rows.push(["drawn", chip(`${draw.tone} · ${draw.why}`, draw.tone)]);
    else if (n.narrowed_out) rows.push(["drawn", "narrowed out by the execution map (no run folded yet)"]);
    else rows.push(["drawn", S.board ? "no event yet in this run" : "no run selected"]);
    if (draw && draw.mark) {
      const ev = S.bySeq.get(draw.mark.seq);
      rows.push(["latest event", ev ? eventLink(ev) : `${draw.mark.event_kind} ${draw.mark.event_id}`]);
    }
    if (draw && draw.why === "not_legal") {
      const set = S.board.legal.find((s) => s.grey.includes(n.id));
      if (set) {
        const step = set.step === null ? "" : ` (step ${set.step})`;
        rows.push(["not legal", `legal set${step}: ${set.legal_actions.join(", ")} — event ${set.event_id}`]);
      }
    }
    box.append(kv(rows));

    box.append(el("h3", "Validated config"));
    if (d.config) {
      box.append(kv([["section", d.config.section], ["source", d.source]]), pre(d.config.value));
    } else {
      box.append(el("p", "No config section stands behind this node (the CLI, or the execution map itself).", "muted"));
    }
    box.append(el("h3", "Graph attrs"), pre({ attrs: n.attrs, facets: n.facets || {}, narrowed_out: n.narrowed_out }));

    box.append(el("h3", "Edges"));
    const ul = el("ul", null, "links");
    for (const e of d.edges.in) {
      const li = el("li");
      li.append(linkish(e.from, () => selectNode(e.from)), document.createTextNode(` —${e.kind}→ this`));
      ul.append(li);
    }
    for (const e of d.edges.out) {
      const li = el("li");
      li.append(document.createTextNode(`this —${e.kind}→ `), linkish(e.to, () => selectNode(e.to)));
      ul.append(li);
    }
    box.append(ul);

    box.append(el("h3", "Evidence"));
    box.append(kv(d.evidence.map((e) => [e.label, e.key ? `${e.path} · key ${e.key}` : e.path])));

    const mine = S.events.filter((e) => e.node_id === n.id && (S.upto === null || e.seq <= S.upto));
    box.append(el("h3", `Events of this node in this run (${mine.length})`));
    const list = el("ul", null, "links");
    for (const e of mine.slice().reverse().slice(0, 50)) {
      const li = el("li");
      li.append(eventLink(e));
      list.append(li);
    }
    if (mine.length > 50) list.append(el("li", `${mine.length - 50} older: filter the timeline by this node's facets`, "muted"));
    box.append(list);
  }

  function answersTable(answers) {
    const t = el("table", null, "answers");
    const head = el("tr");
    for (const h of ["question", "choice", "confidence", "probabilities"]) head.append(el("th", h));
    t.append(head);
    for (const [q, a] of Object.entries(answers)) {
      const tr = el("tr");
      const probs = a && a.probabilities ? Object.entries(a.probabilities).map(([k, p]) => `${k} ${p}`).join(" · ") : "";
      tr.append(el("td", q), el("td", a ? a.choice : ""), el("td", a ? a.confidence : ""), el("td", probs));
      t.append(tr);
    }
    return t;
  }

  function eventDetail(box, ev) {
    const v = ev.view || {};
    const title = el("div", null, "title");
    title.append(el("h3", ev.kind), chip(ev.status, v.tone));
    box.append(title);
    const node = ev.node_id ? linkish(ev.node_id, () => selectNode(ev.node_id)) : null;
    const parent = ev.parent_event_id
      ? linkish(ev.parent_event_id, () => {
          if (findEvent(ev.parent_event_id)) selectEvent(ev.parent_event_id);
        })
      : null;
    box.append(
      kv([
        ["event_id", ev.event_id],
        ["seq", ev.seq],
        ["time", isoTime(ev.ts_ms)],
        ["node", node],
        ["in this graph", ev.node_id ? String(Boolean(v.in_graph)) : null],
        ["session_id", ev.session_id],
        ["correlation_id", ev.correlation_id],
        ["parent", parent],
        ["call_id", ev.call_id],
        ["duration_ms", ev.duration_ms],
        ["decision_id", ev.payload && ev.payload.decision_id],
        ["component", ev.component],
        ["run_id", ev.run_id],
        ["runtime_id", ev.runtime_id],
        ["config_hash", ev.config_hash],
      ]),
    );
    const facets = FACETS.concat(["action", "feed"]).filter((k) => v[k]);
    if (facets.length) box.append(el("h3", "Belongs to"), kv(facets.map((k) => [k, v[k]])));
    if (v.edges && v.edges.length) {
      box.append(el("h3", "Highlights"));
      box.append(kv(v.edges.map((e) => [e.kind, `${e.from} → ${e.to}`])));
    }
    const p = ev.payload || {};
    if (p.answers && typeof p.answers === "object") box.append(el("h3", "Jev answers"), answersTable(p.answers));
    if (p.legal_actions) box.append(el("h3", "Legal this step"), pre({ legal_actions: p.legal_actions, legal: p.legal, questions: p.questions }));
    for (const k of ["args", "output", "line1", "error", "obs", "observation", "outcome", "event", "stats"]) {
      if (p[k] !== undefined && p[k] !== null) box.append(el("h3", k), pre(p[k]));
    }
    box.append(el("h3", "Payload (redacted, as written)"), pre(p));
    if (ev.artifact) box.append(el("h3", "Full record"), kv([["file", ev.artifact.file], ["key", ev.artifact.key]]));
  }

  function renderEvidence() {
    const dl = $("evidence");
    dl.replaceChildren();
    const add = (k, v) => {
      if (v) dl.append(el("dt", k), el("dd", v));
    };
    const ev = S.meta ? S.meta.evidence : {};
    add("trace recordings", ev.trace_dir);
    add("this run's trace file", S.run && S.run.trace_file);
    add("heartbeat (doctor --live)", ev.heartbeat);
    add("decision audit", ev.decisions);
    add("kept execution maps", ev.maps_dir);
    if (S.graphKey) add("execution map drawn", S.graphKey);
    const sel = S.sel.event ? findEvent(S.sel.event) : null;
    if (sel && sel.artifact) add("selected event's record", `${sel.artifact.file}${sel.artifact.key ? ` · key ${sel.artifact.key}` : ""}`);
    if (sel && sel.payload && sel.payload.key) add("observation key", sel.payload.key);
    if (S.inspect === "node" && S.nodeDetail && S.nodeDetail.evidence) {
      for (const e of S.nodeDetail.evidence) add(`node: ${e.label}`, e.key ? `${e.path} · key ${e.key}` : e.path);
    }
  }

  // ---- control (ST-30): state + allowed actions from GET /api/v1/control --

  async function loadControl() {
    S.control = await api("/api/v1/control");
    renderControl();
  }

  function renderControl() {
    const c = S.control;
    const box = $("controls");
    box.hidden = !(c && c.enabled);
    const note = $("ctl-note");
    if (box.hidden) {
      note.hidden = true;
      return;
    }
    const badge = $("ctl-state");
    badge.textContent = c.state;
    badge.className = `badge t-${c.tone}`;
    badge.title = c.own ? `this Studio runs ${c.own.holder}` : c.seen ? `heartbeat: ${c.seen.holder}` : c.why;
    const by = new Map(c.actions.map((a) => [a.action, a]));
    for (const [id, action] of [
      ["btn-play", "play"],
      ["btn-stop", "stop"],
      ["btn-event", "event"],
    ]) {
      const a = by.get(action);
      const b = $(id);
      b.disabled = S.ctlBusy || !a || !a.ok;
      b.title = S.ctlBusy ? "waiting for the answer" : a && !a.ok ? a.why_not : "";
    }
    const sel = $("ctl-scenario");
    const names = c.scenarios.map((s) => s.name);
    if (names.join("\n") !== [...sel.options].map((o) => o.value).join("\n")) {
      const keep = sel.value;
      sel.replaceChildren(...names.map((n) => new Option(n, n)));
      if (names.includes(keep)) sel.value = keep;
    }
    const pick = c.scenarios.find((s) => s.name === sel.value);
    sel.title = pick ? `${pick.file}: ${JSON.stringify(pick.event)}` : "";
    const last = c.last;
    note.hidden = !last;
    if (last) {
      note.className = `ctl-note t-${last.tone || "none"}`;
      note.textContent = `${last.action} · ${last.status} · ${last.detail}` + (last.event_id ? ` · ${last.event_id}` : "");
    }
  }

  async function control(action, body) {
    S.ctlBusy = true;
    renderControl();
    try {
      const r = await post(`/api/v1/control/${action}`, body);
      if (r.body.control) S.control = r.body.control;
      else if (r.body.error) notice(`${action}: ${r.status} ${r.body.error}`);
      if (action === "play" && r.body.ok) goLive();
    } catch (e) {
      notice(String(e.message || e));
    } finally {
      S.ctlBusy = false;
      renderControl();
    }
    await poll();
  }

  // ---- wiring --------------------------------------------------------------

  function wire() {
    $("btn-live").addEventListener("click", () => goLive());
    $("btn-replay").addEventListener("click", () => {
      const pick = S.runId || (S.runs.length ? S.runs[S.runs.length - 1].run_id : null);
      if (pick) openReplay(pick, null);
      else notice("No recorded run yet: start `tengu run` or `tengu decide` for this sandbox.");
    });
    $("r-seq").addEventListener("input", (e) => seek(Number(e.target.value)));
    $("r-first").addEventListener("click", () => seek(0));
    $("r-prev").addEventListener("click", () => seek((S.upto === null ? S.lastSeq : S.upto) - 1));
    $("r-next").addEventListener("click", () => seek((S.upto === null ? S.lastSeq : S.upto) + 1));
    $("r-last").addEventListener("click", () => seek(S.lastSeq));
    $("filters").addEventListener("input", () => renderTimeline());
    $("filters").addEventListener("reset", () => setTimeout(renderTimeline, 0));
    $("filters").addEventListener("submit", (e) => e.preventDefault());
    window.addEventListener("resize", fitGraph);
    $("btn-play").addEventListener("click", () => control("play", {}));
    $("btn-stop").addEventListener("click", () => control("stop", {}));
    $("btn-event").addEventListener("click", () => control("event", { scenario: $("ctl-scenario").value }));
    $("ctl-scenario").addEventListener("change", renderControl);
  }

  async function poll() {
    try {
      await Promise.all([loadHealth(), loadRuns(), loadControl()]);
    } catch (e) {
      notice(String(e.message || e));
    }
  }

  async function start() {
    const h = parseHash();
    S.token = readToken(h);
    if (!S.token) {
      notice("No Studio token: open the exact URL `tengu studio` printed (it ends in #t=…).");
      return;
    }
    wire();
    try {
      await loadMeta();
      await ensureGraph(null);
    } catch (e) {
      notice(String(e.message || e));
      return;
    }
    await poll();
    setInterval(poll, POLL_MS);
    if (h.node) {
      S.sel.node = h.node;
      if (h.show === "node") selectNode(h.node);
    }
    if (h.mode === "replay" && h.run && /^[0-9a-f-]{36}$/.test(h.run)) {
      const at = /^\d+$/.test(h.at || "") ? Number(h.at) : null;
      await openReplay(h.run, at);
    } else {
      goLive();
    }
    if (h.ev && h.show === "event" && findEvent(h.ev)) {
      S.sel.event = h.ev;
      S.inspect = "event";
      renderAll();
    }
    writeHash();
  }

  start();
})();
