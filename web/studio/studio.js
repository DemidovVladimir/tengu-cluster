// Tengu Studio — read-only shell (ST-20). Draws what /api/v1 serves and
// decides nothing: every rule (health verdict, run liveness, config match)
// arrives computed by Rust. The graph, inspector and timeline land in ST-21.
"use strict";

(() => {
  const KEEP_EVENTS = 200;
  const POLL_MS = 5000;
  const $ = (id) => document.getElementById(id);

  // The token rides in the URL fragment (`#t=<hex>`), which a browser never
  // sends to a server; kept for this tab so a reload still works.
  function readToken() {
    const m = /(?:^#|&)t=([0-9a-f]{64})/.exec(location.hash);
    try {
      if (m) sessionStorage.setItem("studio-token", m[1]);
      return m ? m[1] : sessionStorage.getItem("studio-token");
    } catch (_) {
      return m ? m[1] : null;
    }
  }
  const TOKEN = readToken();

  function notice(text) {
    const n = $("notice");
    n.textContent = text || "";
    n.hidden = !text;
  }

  function el(tag, text, cls) {
    const e = document.createElement(tag);
    if (text !== undefined && text !== null) e.textContent = String(text);
    if (cls) e.className = cls;
    return e;
  }

  function time(ms) {
    return ms ? new Date(ms).toISOString().replace("T", " ").replace("Z", " UTC") : "—";
  }

  async function api(path) {
    const r = await fetch(path, { headers: { "X-Studio-Token": TOKEN }, cache: "no-store" });
    const body = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(`${path}: ${r.status} ${body.error || r.statusText}`);
    return body;
  }

  async function meta() {
    const m = await api("/api/v1/meta");
    document.title = `Tengu Studio · ${m.sandbox}`;
    $("sandbox").textContent = m.sandbox;
    $("config-hash").textContent = m.config_hash || "none (no config file)";
    $("mode").textContent = m.read_only ? "read-only" : "control";
    const ev = $("evidence");
    ev.replaceChildren();
    for (const [k, v] of Object.entries(m.evidence || {})) ev.append(el("dt", k), el("dd", v));
  }

  async function health() {
    const h = await api("/api/v1/health");
    const v = $("health");
    v.textContent = h.live ? "live" : "not live";
    v.className = h.live ? "verdict-ok" : "verdict-bad";
    const list = $("checks");
    list.replaceChildren();
    for (const c of h.checks) {
      const li = el("li", null, c.ok ? "check-ok" : "check-bad");
      li.append(el("span", c.ok ? "ok" : "FAIL", "mark"), el("span", c.subject), el("span", c.detail, "detail"));
      list.append(li);
    }
  }

  async function runs() {
    const r = await api("/api/v1/runs");
    const body = $("runs");
    body.replaceChildren();
    for (const run of r.runs.slice().reverse()) {
      const tr = el("tr", null, run.run_id === r.live_run_id ? "live" : "");
      for (const cell of [
        run.run_id,
        run.kind || "?",
        run.runtime_id || "—",
        time(run.started_ms),
        run.events,
        `${run.last_kind} (${run.last_status})`,
        run.config_current ? "current" : "changed",
      ]) tr.append(el("td", cell));
      body.append(tr);
    }
  }

  async function graph() {
    const g = await api("/api/v1/graph");
    $("graph").textContent =
      `${g.nodes.length} nodes · ${g.edges.length} edges (workflow schema ${g.schema_version}); ` +
      "drawn in ST-21 — the JSON: GET /api/v1/graph";
  }

  function live() {
    const src = new EventSource(`/api/v1/live/stream?token=${encodeURIComponent(TOKEN)}`);
    const list = $("live-events");
    src.addEventListener("run", (e) => {
      const a = JSON.parse(e.data);
      $("live-run").textContent = a.run_id
        ? `${a.reason}: run ${a.run_id} · runtime ${a.runtime_id}`
        : `${a.reason}: ${a.detail}`;
      if (a.reason === "restarted") list.replaceChildren();
    });
    src.addEventListener("trace", (e) => {
      const ev = JSON.parse(e.data);
      const took = ev.duration_ms === null || ev.duration_ms === undefined ? "" : ` ${ev.duration_ms} ms`;
      list.prepend(el("li", `#${ev.seq} ${time(ev.ts_ms)} ${ev.kind} ${ev.node_id || "-"} ${ev.status}${took}`));
      while (list.children.length > KEEP_EVENTS) list.lastChild.remove();
    });
    // The server closes the stream after `lagged`; the browser reconnects
    // with the last event id and the rest is read from the trace file.
    src.addEventListener("lagged", (e) => {
      const l = JSON.parse(e.data);
      $("live-run").textContent = `lagged after seq ${l.last_seq} of run ${l.run_id}; resuming from the file`;
    });
    src.onerror = () => {
      if (src.readyState === EventSource.CLOSED) notice("Live stream closed by the server — reload the page.");
    };
  }

  async function refresh() {
    try {
      await Promise.all([health(), runs()]);
      notice("");
    } catch (e) {
      notice(String(e.message || e));
    }
  }

  async function start() {
    if (!TOKEN) {
      notice("No Studio token: open the exact URL `tengu studio` printed (it ends in #t=…).");
      return;
    }
    try {
      await meta();
      await graph();
    } catch (e) {
      notice(String(e.message || e));
      return;
    }
    await refresh();
    live();
    setInterval(refresh, POLL_MS);
  }

  start();
})();
