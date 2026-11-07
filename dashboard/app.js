// AI Gateway admin dashboard. No dependencies; every value from the API is
// written with textContent, never as HTML, because logs contain user input.
(function () {
  "use strict";

  const TOKEN_KEY = "aigw-admin-token";
  const RANGES = ["15m", "1h", "6h", "24h", "7d", "30d"];
  const state = {
    token: readToken(),
    range: "24h",
    page: "overview",
    logs: { offset: 0, rows: [], total: 0, live: false, timer: null, filters: { q: "", status: "", tenant: "", model: "", user: "", tag: "", endpoint: "" } },
    keys: [],
  };
  const app = document.getElementById("app");
  const tip = document.getElementById("tooltip");

  // ---------- helpers ----------
  function readToken() {
    try { return sessionStorage.getItem(TOKEN_KEY) || ""; } catch (_) { return ""; }
  }
  function saveToken(t) {
    try { if (t) sessionStorage.setItem(TOKEN_KEY, t); else sessionStorage.removeItem(TOKEN_KEY); } catch (_) {}
    state.token = t;
  }

  function el(tag, attrs, ...children) {
    const n = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === null || v === undefined || v === false) continue;
      if (k === "class") n.className = v;
      else if (k === "text") n.textContent = v;
      else if (k.startsWith("on")) n.addEventListener(k.slice(2), v);
      else n.setAttribute(k, v === true ? "" : String(v));
    }
    for (const c of children.flat(Infinity)) {
      if (c === null || c === undefined || c === false) continue;
      n.append(c instanceof Node ? c : document.createTextNode(String(c)));
    }
    return n;
  }

  const nf = new Intl.NumberFormat();
  function num(n) { return n === null || n === undefined ? "–" : nf.format(n); }
  function compact(n) {
    if (n === null || n === undefined) return "–";
    const a = Math.abs(n);
    if (a >= 1e9) return (n / 1e9).toFixed(1) + "B";
    if (a >= 1e6) return (n / 1e6).toFixed(1) + "M";
    if (a >= 1e4) return (n / 1e3).toFixed(1) + "k";
    return nf.format(n);
  }
  function usd(n) {
    if (n === null || n === undefined) return "–";
    if (n === 0) return "$0";
    if (n < 0.01) return "$" + n.toFixed(4);
    return "$" + n.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  }
  function pct(x) { return (x * 100).toFixed(x > 0 && x < 0.01 ? 2 : 1) + "%"; }
  function ms(n) {
    if (n === null || n === undefined) return "–";
    return n >= 10000 ? (n / 1000).toFixed(1) + " s" : nf.format(n) + " ms";
  }
  function when(tsMs) {
    const d = new Date(tsMs);
    const today = new Date();
    const time = d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" });
    return d.toDateString() === today.toDateString() ? time : d.toLocaleDateString() + " " + time;
  }

  class ApiError extends Error {
    constructor(status, message, code) { super(message); this.status = status; this.code = code; }
  }

  async function api(path, opts) {
    const res = await fetch(path, {
      ...(opts || {}),
      headers: { Authorization: "Bearer " + state.token, "Content-Type": "application/json" },
    });
    if (res.status === 204) return null;
    let body = null;
    try { body = await res.json(); } catch (_) {}
    if (!res.ok) {
      const e = body && body.error ? body.error : {};
      throw new ApiError(res.status, e.message || "Request failed with status " + res.status, e.code);
    }
    return body;
  }

  function handleError(err, target) {
    if (err instanceof ApiError && err.status === 401) {
      saveToken("");
      renderLogin("That admin token was not accepted. Enter it again.");
      return;
    }
    if (err instanceof ApiError && err.code === "admin_disabled") {
      saveToken("");
      renderLogin("The admin API is turned off. Set admin.token_sha256 in the gateway config and restart.");
      return;
    }
    const msg = err && err.message ? err.message : "Something went wrong.";
    (target || app).replaceChildren(el("div", { class: "notice err", text: msg }));
  }

  // ---------- login ----------
  function renderLogin(message) {
    stopLive();
    const input = el("input", { type: "password", id: "token", autocomplete: "current-password", required: true });
    app.replaceChildren(
      el("div", { class: "login" },
        el("h1", { text: "AI Gateway" }),
        el("p", { class: "sub", text: "Sign in with the admin token. Its SHA-256 is admin.token_sha256 in the gateway config." }),
        message ? el("div", { class: "notice err", text: message }) : null,
        el("form", {
          onsubmit: (e) => {
            e.preventDefault();
            saveToken(input.value.trim());
            route();
          },
        },
          el("label", { class: "field", for: "token" }, "Admin token", input),
          el("button", { class: "btn primary", type: "submit", text: "Sign in" })
        )
      )
    );
    input.focus();
  }

  // ---------- shell ----------
  const PAGES = [
    ["overview", "Overview"],
    ["logs", "Requests"],
    ["models", "Models"],
    ["reports", "Usage reports"],
    ["keys", "API keys"],
    ["rules", "Routing rules"],
    ["limits", "Limits"],
    ["setup", "Providers & routes"],
    ["audit", "Audit log"],
  ];

  function shell(content) {
    const nav = el("nav", { class: "rail", "aria-label": "Sections" },
      el("div", { class: "brand" }, "AI Gateway", el("small", { text: location.host })),
      PAGES.map(([id, label]) =>
        el("a", { href: "#" + id, "aria-current": state.page === id ? "page" : null, text: label })),
      el("div", { class: "spacer" }),
      el("button", { class: "link", type: "button", text: "Sign out", onclick: () => { saveToken(""); renderLogin(); } })
    );
    const main = el("main", { id: "main" }, content);
    app.replaceChildren(el("div", { class: "shell" }, nav, main));
    return main;
  }

  function rangePicker(onChange) {
    return el("div", { class: "seg", role: "group", "aria-label": "Time range" },
      RANGES.map((r) => el("button", {
        type: "button", "aria-pressed": state.range === r ? "true" : "false", text: r,
        onclick: () => { state.range = r; onChange(); },
      })));
  }

  // ---------- charts ----------
  function setupCanvas(canvas, height) {
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || canvas.parentElement.clientWidth;
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(height * dpr);
    canvas.style.height = height + "px";
    const ctx = canvas.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    return { ctx, w, h: height };
  }

  function cssVar(name) { return getComputedStyle(document.documentElement).getPropertyValue(name).trim(); }

  function niceMax(v) {
    if (v <= 0) return 1;
    const p = Math.pow(10, Math.floor(Math.log10(v)));
    for (const m of [1, 2, 2.5, 5, 10]) if (m * p >= v) return m * p;
    return 10 * p;
  }

  function tickLabel(t, step) {
    const d = new Date(t * 1000);
    if (step >= 3600 && state.range !== "6h" && state.range !== "24h") {
      return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
    }
    return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  }

  // series: [{ key, color, label }]; kind: "bars" (stacked) or "line".
  function drawChart(canvas, data, series, opts) {
    const height = opts.height || 180;
    const { ctx, w, h } = setupCanvas(canvas, height);
    const stacked = opts.kind === "bars";
    const totals = data.map((d) => stacked
      ? series.reduce((a, s) => a + (d[s.key] || 0), 0)
      : Math.max(...series.map((s) => d[s.key] || 0)));
    const max = niceMax(Math.max(0, ...totals));
    const fmt = opts.format || compact;
    ctx.clearRect(0, 0, w, h);
    ctx.font = "11px system-ui, sans-serif";
    let labelW = 0;
    for (let i = 0; i <= 4; i++) labelW = Math.max(labelW, ctx.measureText(fmt((max * i) / 4)).width);
    const pad = { l: Math.ceil(labelW) + 12, r: 8, t: 8, b: 22 };
    const iw = w - pad.l - pad.r, ih = h - pad.t - pad.b;
    ctx.fillStyle = cssVar("--muted");
    ctx.strokeStyle = cssVar("--rule");
    ctx.lineWidth = 1;
    for (let i = 0; i <= 4; i++) {
      const y = Math.round(pad.t + ih - (ih * i) / 4) + 0.5;
      ctx.beginPath(); ctx.moveTo(pad.l, y); ctx.lineTo(w - pad.r, y); ctx.stroke();
      ctx.textAlign = "right"; ctx.textBaseline = "middle";
      ctx.fillText(fmt((max * i) / 4), pad.l - 6, y);
    }
    const n = data.length;
    if (n === 0) return;
    const slot = iw / n;
    ctx.textAlign = "center"; ctx.textBaseline = "top";
    const every = Math.max(1, Math.ceil(n / Math.max(2, Math.floor(iw / 90))));
    for (let i = 0; i < n; i += every) {
      ctx.fillText(tickLabel(data[i].t, opts.step), pad.l + slot * (i + 0.5), h - pad.b + 6);
    }
    if (stacked) {
      const bw = Math.max(1, slot - (slot > 6 ? 2 : 0.5));
      data.forEach((d, i) => {
        let y = pad.t + ih;
        for (const s of series) {
          const v = d[s.key] || 0;
          if (!v) continue;
          const bh = (v / max) * ih;
          ctx.fillStyle = cssVar(s.color);
          ctx.fillRect(pad.l + slot * i + (slot - bw) / 2, y - bh, bw, bh);
          y -= bh;
        }
      });
    } else {
      for (const s of series) {
        ctx.strokeStyle = cssVar(s.color);
        ctx.lineWidth = 1.75;
        ctx.beginPath();
        data.forEach((d, i) => {
          const x = pad.l + slot * (i + 0.5);
          const y = pad.t + ih - ((d[s.key] || 0) / max) * ih;
          if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
        });
        ctx.stroke();
      }
    }
    canvas.onmousemove = (e) => {
      const r = canvas.getBoundingClientRect();
      const i = Math.floor((e.clientX - r.left - pad.l) / slot);
      if (i < 0 || i >= n) { tip.classList.add("hidden"); return; }
      const d = data[i];
      tip.textContent = new Date(d.t * 1000).toLocaleString() + "  ·  " +
        series.map((s) => s.label + " " + fmt(d[s.key] || 0)).join("  ·  ");
      tip.style.left = Math.min(e.clientX + 12, window.innerWidth - tip.offsetWidth - 8) + "px";
      tip.style.top = (e.clientY + 14) + "px";
      tip.classList.remove("hidden");
    };
    canvas.onmouseleave = () => tip.classList.add("hidden");
  }

  function legend(series) {
    return el("div", { class: "legend" }, series.map((s) => {
      const sw = el("i");
      sw.style.background = "var(" + s.color + ")";
      return el("span", {}, sw, s.label);
    }));
  }

  function chartPanel(title, data, series, opts) {
    const canvas = el("canvas", { class: "chart", role: "img", "aria-label": title });
    const panel = el("section", { class: "panel" },
      el("div", { class: "strip-head" }, el("h2", { text: title }), series.length > 1 ? legend(series) : null),
      canvas);
    requestAnimationFrame(() => drawChart(canvas, data, series, opts));
    charts.push(() => drawChart(canvas, data, series, opts));
    return panel;
  }

  let charts = [];
  window.addEventListener("resize", () => charts.forEach((f) => f()));

  // ---------- overview ----------
  async function renderOverview() {
    charts = [];
    const main = shell(el("div", { class: "empty", text: "Loading…" }));
    let d;
    try { d = await api("/api/overview?range=" + state.range); } catch (e) { return handleError(e, main); }
    const t = d.totals;
    const head = el("div", { class: "page-head" },
      el("div", {}, el("h1", { text: "Overview" }),
        el("div", { class: "sub", text: "Traffic through the gateway, all replicas, last " + state.range + "." })),
      el("div", { class: "filters" }, rangePicker(renderOverview),
        el("button", { class: "btn", type: "button", text: "Refresh", onclick: renderOverview })));

    const trafficSeries = [
      { key: "ok", color: "--traffic", label: "Succeeded" },
      { key: "err", color: "--error", label: "Failed" },
    ];
    const series = d.series.map((p) => ({ ...p, ok: Math.max(0, p.req - p.err) }));
    const canvas = el("canvas", { class: "chart", role: "img", "aria-label": "Requests over time" });
    const strip = el("section", { class: "strip" },
      el("div", { class: "strip-head" },
        el("h2", { text: "Requests per " + (d.step_secs >= 3600 ? "hour" : "minute") }), legend(trafficSeries)),
      canvas,
      el("div", { class: "band" },
        fig("Requests", compact(t.requests), t.cache_hits ? pct(t.cache_hit_rate) + " served from cache" : "No cache hits"),
        fig("Failed", pct(t.error_rate), num(t.errors) + " requests", t.error_rate >= 0.05),
        fig("Tokens", compact(t.prompt_tokens + t.completion_tokens), compact(t.prompt_tokens) + " in, " + compact(t.completion_tokens) + " out"),
        fig("Spend", usd(t.cost_usd), "Provider-reported or configured prices"),
        fig("Latency p50", ms(t.p50_latency_ms), "p95 " + ms(t.p95_latency_ms) + ", p99 " + ms(t.p99_latency_ms)),
        fig("First token p50", ms(t.p50_ttft_ms), "p95 " + ms(t.p95_ttft_ms)),
        fig("Guardrail hits", num(t.guardrail_hits), num(t.feedback) + " feedback ratings")));
    requestAnimationFrame(() => drawChart(canvas, series, trafficSeries, { kind: "bars", step: d.step_secs, height: 200 }));
    charts.push(() => drawChart(canvas, series, trafficSeries, { kind: "bars", step: d.step_secs, height: 200 }));

    const charts2 = el("div", { class: "grid2" },
      chartPanel("Tokens", d.series, [{ key: "tokens", color: "--tokens", label: "Tokens" }], { kind: "line", step: d.step_secs }),
      chartPanel("Spend", d.series, [{ key: "cost_usd", color: "--cost", label: "Spend" }], { kind: "bars", step: d.step_secs, format: usd }),
      chartPanel("Latency and time to first token", d.series, [
        { key: "avg_latency_ms", color: "--traffic", label: "Latency" },
        { key: "avg_ttft_ms", color: "--tokens", label: "First token" },
      ], { kind: "line", step: d.step_secs, format: ms }),
      breakdownPanel("Status codes", d.statuses, [], t.requests));

    const models = d.models.map((m) => ({
      ...m,
      avg: m.requests ? Math.round(m.latency_sum_ms / m.requests) : 0,
      rating: (m.thumbs_up || m.thumbs_down) ? Math.round((100 * m.thumbs_up) / (m.thumbs_up + m.thumbs_down)) + "% 👍" : "–",
    }));
    const tables = el("div", { class: "grid2" },
      breakdownPanel("Models", models, [["Tokens", "tokens", compact], ["Spend", "cost_usd", usd], ["Failed", "errors", num], ["Avg latency", "avg", ms], ["Feedback", "rating", (x) => x]], t.requests, true),
      breakdownPanel("Users", d.users, [["Tokens", "tokens", compact], ["Spend", "cost_usd", usd]], t.requests),
      breakdownPanel("Tags", d.tags, [["Spend", "cost_usd", usd]], t.requests),
      breakdownPanel("APIs", d.endpoints, [], t.requests),
      breakdownPanel("API keys", d.keys, [["Tokens", "tokens", compact], ["Spend", "cost_usd", usd]], t.requests),
      breakdownPanel("Providers", d.providers.map((p) => ({ ...p, avg: p.requests ? Math.round(p.latency_sum_ms / p.requests) : 0 })),
        [["Failed", "errors", num], ["Failed attempts", "failed_attempts", (v) => num(v || 0)], ["Avg latency", "avg", ms]], t.requests));

    main.replaceChildren(head, strip, charts2, tables);
    if (!t.requests) {
      main.insertBefore(el("div", { class: "notice", text: "No requests in this range yet. Send one to /v1/chat/completions and it will show up here." }), strip);
    }
  }

  function fig(label, value, detail, bad) {
    return el("div", { class: "fig" + (bad ? " bad" : "") },
      el("div", { class: "k", text: label }), el("div", { class: "v", text: value }), el("div", { class: "d", text: detail }));
  }

  function breakdownPanel(title, rows, cols, total, wide) {
    const cls = wide ? " wide" : "";
    if (!rows.length) return el("section", { class: "panel" + cls }, el("h2", { text: title }), el("div", { class: "empty", text: "Nothing yet." }));
    const max = Math.max(...rows.map((r) => r.requests));
    return el("section", { class: "panel flush" + cls }, el("h2", { text: title }),
      el("div", { class: "table-wrap" }, el("table", {},
        el("thead", {}, el("tr", {}, el("th", { text: "Name" }), el("th", { class: "num", text: "Requests" }),
          cols.map(([label]) => el("th", { class: "num", text: label })))),
        el("tbody", {}, rows.map((r) => {
          const b = el("span");
          b.style.width = Math.max(2, (r.requests / max) * 100) + "%";
          const warn = title === "Status codes" && Number(r.name) >= 400;
          return el("tr", {},
            el("td", { class: "trunc", title: r.name }, r.name, el("div", { class: "bar" + (warn ? " warn" : "") }, b)),
            el("td", { class: "num", text: num(r.requests) + (total ? "  (" + pct(r.requests / total) + ")" : "") }),
            cols.map(([, key, f]) => el("td", { class: "num", text: f(r[key]) })));
        })))));
  }

  // ---------- logs ----------
  function stopLive() {
    if (state.logs.timer) clearInterval(state.logs.timer);
    state.logs.timer = null;
  }

  async function renderLogs() {
    charts = [];
    const f = state.logs.filters;
    const search = el("input", { type: "search", placeholder: "Search id, error, model, key, guardrail", value: f.q, "aria-label": "Search requests" });
    const status = el("select", { "aria-label": "Status" },
      [["", "All statuses"], ["ok", "Succeeded"], ["error", "Failed"], ["cached", "From cache"], ["flagged", "Guardrail matched"]].map(([v, l]) =>
        el("option", { value: v, selected: f.status === v, text: l })));
    const tenant = el("select", { "aria-label": "API key" }, el("option", { value: "", text: "All keys" }));
    const model = el("input", { type: "text", placeholder: "Model or route", value: f.model, "aria-label": "Model" });
    const user = el("input", { type: "text", placeholder: "User", value: f.user, "aria-label": "User" });
    const tag = el("input", { type: "text", placeholder: "Tag", value: f.tag, "aria-label": "Tag" });
    const endpoint = el("select", { "aria-label": "API" },
      [["", "All APIs"], ["chat", "Chat"], ["responses", "Responses"], ["messages", "Messages"], ["embeddings", "Embeddings"],
        ["images", "Images"], ["moderations", "Moderations"], ["rerank", "Rerank"], ["speech", "Speech"], ["native", "Native"], ["realtime", "Realtime"]]
        .map(([v, l]) => el("option", { value: v, selected: f.endpoint === v, text: l })));
    const live = el("input", { type: "checkbox", checked: state.logs.live });
    const apply = () => {
      Object.assign(f, { q: search.value, status: status.value, tenant: tenant.value, model: model.value, user: user.value, tag: tag.value, endpoint: endpoint.value });
      state.logs.offset = 0;
      loadLogs(false);
    };
    let debounce;
    search.addEventListener("input", () => { clearTimeout(debounce); debounce = setTimeout(apply, 250); });
    for (const i of [model, user, tag]) i.addEventListener("input", () => { clearTimeout(debounce); debounce = setTimeout(apply, 250); });
    endpoint.addEventListener("change", apply);
    status.addEventListener("change", apply);
    tenant.addEventListener("change", apply);
    live.addEventListener("change", () => {
      state.logs.live = live.checked;
      stopLive();
      if (live.checked) state.logs.timer = setInterval(() => { if (state.logs.offset === 0) loadLogs(false, true); }, 5000);
    });
    if (state.logs.live) {
      stopLive();
      state.logs.timer = setInterval(() => loadLogs(false, true), 5000);
    }

    const tableHost = el("div", { id: "log-table" }, el("div", { class: "empty", text: "Loading…" }));
    const main = shell([
      el("div", { class: "page-head" },
        el("div", {}, el("h1", { text: "Requests" }), el("div", { class: "sub", id: "log-count" })),
        el("div", { class: "filters" },
          el("label", { class: "toggle" }, live, "Update every 5 seconds"),
          el("button", { class: "btn danger", type: "button", text: "Delete matching", onclick: deleteMatching }))),
      el("div", { class: "filters" }, search, status, tenant, model, user, tag, endpoint),
      tableHost,
    ]);

    api("/api/keys").then((k) => {
      state.keys = k.data;
      for (const key of k.data) tenant.append(el("option", { value: key.name, selected: f.tenant === key.name, text: key.name }));
    }).catch(() => {});
    await loadLogs(false);
    void main;
  }

  async function deleteMatching() {
    const f = state.logs.filters;
    const qs = new URLSearchParams();
    for (const [k, v] of Object.entries(f)) if (v) qs.set(k, v);
    const filtered = [...qs.keys()].length > 0;
    if (!confirm(filtered ? "Delete every logged request that matches these filters?" : "Delete every logged request? Analytics and reports are kept.")) return;
    try {
      const r = await api("/api/logs" + (filtered ? "?" + qs : ""), { method: "DELETE" });
      alert("Deleted " + num(r.deleted) + " requests from the log.");
    } catch (e) { alert(e.message); }
    loadLogs(false);
  }

  async function loadLogs(append, quiet) {
    const host = document.getElementById("log-table");
    if (!host) return;
    const f = state.logs.filters;
    const qs = new URLSearchParams({ limit: "100", offset: String(append ? state.logs.offset : 0) });
    for (const [k, v] of Object.entries(f)) if (v) qs.set(k, v);
    let res;
    try { res = await api("/api/logs?" + qs); } catch (e) { if (!quiet) handleError(e, host); return; }
    state.logs.rows = append ? state.logs.rows.concat(res.data) : res.data;
    state.logs.total = res.total;
    state.logs.offset = state.logs.rows.length;
    const count = document.getElementById("log-count");
    if (count) count.textContent = num(res.total) + " matching requests in the log" + (res.capture_bodies ? "" : ". Prompts and responses are not captured (logging.capture_bodies is off).");
    drawLogTable(host);
  }

  function drawLogTable(host) {
    const rows = state.logs.rows;
    if (!rows.length) {
      host.replaceChildren(el("div", { class: "panel empty", text: "No requests match these filters." }));
      return;
    }
    const table = el("table", {},
      el("thead", {}, el("tr", {},
        ["Time", "Status", "Key", "Model", "Provider", "Tokens", "Spend", "Latency", ""].map((h, i) =>
          el("th", { class: i >= 5 && i <= 7 ? "num" : null, text: h })))),
      el("tbody", {}, rows.map((r) => el("tr", {
        class: "click", tabindex: "0",
        onclick: () => openLog(r.id),
        onkeydown: (e) => { if (e.key === "Enter") openLog(r.id); },
      },
        el("td", { text: when(r.ts_ms) }),
        el("td", {}, el("span", { class: "pill " + (r.status >= 400 || r.status === 0 ? "err" : "ok"), text: String(r.status || "–") })),
        el("td", { text: r.tenant }),
        el("td", { class: "trunc", title: r.model || r.route || "", text: r.model || r.route || "–" }),
        el("td", { text: r.provider || "–" }),
        el("td", { class: "num", text: r.cache_hit ? "cached" : compact(r.prompt_tokens + r.completion_tokens) }),
        el("td", { class: "num", text: r.cost_usd ? usd(r.cost_usd) : "–" }),
        el("td", { class: "num", text: ms(r.latency_ms) }),
        el("td", {},
          r.stream ? el("span", { class: "pill", text: "stream" }) : null, " ",
          r.endpoint && r.endpoint !== "chat" ? el("span", { class: "pill", text: r.endpoint }) : null, " ",
          r.attempts_count > 1 ? el("span", { class: "pill flag", text: r.attempts_count + " tries" }) : null, " ",
          (r.guardrails || []).length ? el("span", { class: "pill flag", title: r.guardrails.join(", "), text: "guardrail" }) : null)))));
    const more = state.logs.rows.length < state.logs.total
      ? el("div", { class: "actions" }, el("button", { class: "btn", type: "button", text: "Show more", onclick: () => loadLogs(true) }))
      : null;
    host.replaceChildren(...[el("section", { class: "panel flush" }, el("div", { class: "table-wrap" }, table)), more].filter(Boolean));
  }

  function prettyBody(text) {
    if (!text) return text;
    try { return JSON.stringify(JSON.parse(text), null, 2); } catch (_) { return text; }
  }

  async function openLog(id) {
    let r;
    try { r = await api("/api/logs/" + encodeURIComponent(id)); } catch (e) { alert(e.message); return; }
    const close = () => { scrim.remove(); drawer.remove(); document.removeEventListener("keydown", esc); };
    const esc = (e) => { if (e.key === "Escape") close(); };
    document.addEventListener("keydown", esc);
    const scrim = el("div", { class: "scrim", onclick: close });
    const meta = Object.entries(r.metadata || {}).map(([k, v]) => k + "=" + v).join(", ");
    const rows = [
      ["Request id", r.id], ["Time", new Date(r.ts_ms).toLocaleString()], ["API key", r.tenant],
      ["API", r.endpoint], ["Requested model", r.route || "–"], ["Served by", (r.provider || "–") + " / " + (r.model || "–")],
      ["Status", String(r.status)], ["Streamed", r.stream ? "Yes" : "No"],
      ["Cache", r.cache_hit ? "Served from cache" : "Not cached"],
      ["Tokens", num(r.prompt_tokens) + " in (" + num(r.cached_tokens || 0) + " cached), " + num(r.completion_tokens) + " out" + (r.reasoning_tokens ? " (" + num(r.reasoning_tokens) + " reasoning)" : "")],
      ["Spend", r.cost_usd ? usd(r.cost_usd) : "–"], ["Latency", ms(r.latency_ms)],
      ["First token", ms(r.ttft_ms)], ["Output speed", r.tokens_per_sec ? r.tokens_per_sec.toFixed(1) + " tokens/s" : "–"],
      ["User", r.user || "–"], ["Tags", (r.tags || []).join(", ") || "–"], ["Metadata", meta || "–"],
      ["Event id", r.event_id || "–"],
      ["Provider key", ({ config: "Config file", stored: "Stored in gateway (BYOK)", request: "Sent by client (BYOK)" })[r.credential] || "–"],
      ["Zero data retention", r.zero_data_retention ? "Yes" : "No"],
      ["Guardrails", (r.guardrails || []).join(", ") || "None matched"],
      ["Error", r.error || "None"],
    ];
    const attempts = (r.attempts || []).length ? el("div", { class: "table-wrap" }, el("table", {},
      el("thead", {}, el("tr", {}, ["#", "Provider / model", "Status", "Started", "Took", "Error"].map((h) => el("th", { text: h })))),
      el("tbody", {}, r.attempts.map((a, i) => el("tr", {},
        el("td", { text: String(i + 1) }), el("td", { text: a.provider + " / " + a.model }),
        el("td", {}, el("span", { class: "pill " + (a.status >= 400 || a.status === 0 ? "err" : "ok"), text: String(a.status) })),
        el("td", { text: "+" + ms(a.start_ms) }), el("td", { text: ms(a.latency_ms) }), el("td", { class: "trunc", text: a.error || "" }))))))
      : el("p", { class: "sub", text: r.cache_hit ? "Served from cache; no provider was called." : "No provider attempts recorded." });
    const fb = r.feedback;
    const fbStatus = el("span", { class: "sub", text: fb ? "Rated " + (fb.rating > 0 ? "👍" : fb.rating < 0 ? "👎" : "neutral") + (fb.score !== undefined && fb.score !== null ? ", score " + fb.score : "") + (fb.comment ? ": " + fb.comment : "") : "Not rated yet" });
    const rate = (rating) => async () => {
      try { await api("/api/logs/" + encodeURIComponent(r.id) + "/feedback", { method: "POST", body: JSON.stringify({ rating }) }); fbStatus.textContent = "Saved " + (rating > 0 ? "👍" : "👎"); }
      catch (e) { fbStatus.textContent = e.message; }
    };
    const closeBtn = el("button", { class: "btn", type: "button", text: "Close", onclick: close });
    const drawer = el("aside", { class: "drawer", role: "dialog", "aria-label": "Request detail" },
      el("header", {}, el("div", {}, el("h1", { text: "Request" }), el("div", { class: "sub", text: r.id })), closeBtn),
      el("dl", { class: "kv" }, rows.map(([k, v]) => [el("dt", { text: k }), el("dd", { text: v })])),
      el("h2", { text: "Provider attempts" }), attempts,
      el("h2", { text: "Feedback", style: "margin-top:16px" }),
      el("div", { class: "filters" },
        el("button", { class: "btn", type: "button", text: "👍 Good", onclick: rate(1) }),
        el("button", { class: "btn", type: "button", text: "👎 Bad", onclick: rate(-1) }), fbStatus),
      el("h2", { text: "Prompt" }),
      r.request_body ? el("pre", { class: "body", text: prettyBody(r.request_body) })
        : el("p", { class: "sub", text: "Not captured. Turn on logging.capture_bodies to store prompts and responses." }),
      el("h2", { text: "Response" }),
      r.response_body ? el("pre", { class: "body", text: prettyBody(r.response_body) })
        : el("p", { class: "sub", text: "Not captured." }));
    document.body.append(scrim, drawer);
    closeBtn.focus();
  }

  // ---------- keys ----------
  async function renderKeys() {
    charts = [];
    const main = shell(el("div", { class: "empty", text: "Loading…" }));
    let res;
    try { res = await api("/api/keys"); } catch (e) { return handleError(e, main); }
    state.keys = res.data;
    const head = el("div", { class: "page-head" },
      el("div", {}, el("h1", { text: "API keys" }),
        el("div", { class: "sub", text: "Keys from the config file change with a restart. Keys created here work on every replica within a few seconds." })),
      el("button", { class: "btn primary", type: "button", text: "Create key", onclick: openCreateKey }));
    const table = el("table", {},
      el("thead", {}, el("tr", {}, ["Name", "Defined in", "Allowed models", "Requests/min", "Tokens today", "Concurrency", ""].map((h, i) =>
        el("th", { class: i >= 3 && i <= 5 ? "num" : null, text: h })))),
      el("tbody", {}, res.data.map((k) => {
        const used = k.tokens_used_today;
        let usage = el("span", { text: used === null ? "–" : compact(used) });
        if (k.daily_token_budget) {
          const ratio = Math.min(1, (used || 0) / k.daily_token_budget);
          const b = el("span");
          b.style.width = Math.max(2, ratio * 100) + "%";
          usage = el("div", {}, compact(used || 0) + " of " + compact(k.daily_token_budget), el("div", { class: "bar" + (ratio > 0.9 ? " warn" : "") }, b));
        }
        return el("tr", {},
          el("td", {}, el("div", { text: k.name }), k.key_hint ? el("div", { class: "sub", text: k.key_hint }) : null),
          el("td", {}, k.source === "admin" ? "Dashboard" : "Config file", k.zero_data_retention ? el("span", { class: "pill", style: "margin-left:6px", text: "ZDR" }) : null),
          el("td", { class: "trunc", title: k.allowed_models.join(", "), text: k.allowed_models.join(", ") }),
          el("td", { class: "num", text: k.requests_per_minute ? num(k.requests_per_minute) : "No limit" }),
          el("td", { class: "num" }, usage),
          el("td", { class: "num", text: k.max_concurrent_requests ? num(k.max_concurrent_requests) : "No limit" }),
          el("td", {}, k.source === "admin"
            ? el("button", { class: "btn danger", type: "button", text: "Revoke", onclick: () => revokeKey(k.name) })
            : null));
      })));
    main.replaceChildren(head, el("section", { class: "panel flush" }, el("div", { class: "table-wrap" }, table)));
  }

  async function revokeKey(name) {
    if (!confirm("Revoke the key “" + name + "”? Clients using it will get 401 errors within a few seconds.")) return;
    try { await api("/api/keys/" + encodeURIComponent(name), { method: "DELETE" }); } catch (e) { alert(e.message); return; }
    renderKeys();
  }

  function openCreateKey() {
    const name = el("input", { type: "text", required: true, maxlength: "64", pattern: "[A-Za-z0-9._-]+", autocomplete: "off" });
    const models = el("input", { type: "text", value: "*", required: true });
    const rpm = el("input", { type: "number", min: "1" });
    const budget = el("input", { type: "number", min: "1" });
    const conc = el("input", { type: "number", min: "1" });
    const zdr = el("input", { type: "checkbox" });
    const err = el("div", { class: "notice err hidden" });
    const close = () => { scrim.remove(); modal.remove(); };
    const scrim = el("div", { class: "scrim", onclick: close });
    const optNum = (i) => (i.value ? Number(i.value) : undefined);
    const form = el("form", {
      onsubmit: async (e) => {
        e.preventDefault();
        const body = {
          name: name.value.trim(),
          allowed_models: models.value.split(",").map((s) => s.trim()).filter(Boolean),
          requests_per_minute: optNum(rpm),
          daily_token_budget: optNum(budget),
          max_concurrent_requests: optNum(conc),
          zero_data_retention: zdr.checked,
        };
        let created;
        try { created = await api("/api/keys", { method: "POST", body: JSON.stringify(body) }); }
        catch (x) { err.textContent = x.message; err.classList.remove("hidden"); return; }
        showNewKey(created, modal, close);
      },
    },
      el("h1", { text: "Create an API key" }),
      el("div", { class: "form-grid" },
        el("label", { class: "field wide" }, "Name, shown in logs and metrics", name),
        el("label", { class: "field wide" }, "Allowed models, comma separated (route names, provider/model, provider/* or *)", models),
        el("label", { class: "field" }, "Requests per minute", rpm),
        el("label", { class: "field" }, "Daily token budget", budget),
        el("label", { class: "field" }, "Concurrent requests per replica", conc),
        el("label", { class: "toggle wide" }, zdr, "Zero data retention: never store or cache this key's prompts, and only use ZDR providers")),
      err,
      el("div", { class: "actions" },
        el("button", { class: "btn", type: "button", text: "Cancel", onclick: close }),
        el("button", { class: "btn primary", type: "submit", text: "Create key" })));
    const modal = el("div", { class: "modal", role: "dialog", "aria-label": "Create an API key" }, form);
    document.body.append(scrim, modal);
    name.focus();
  }

  function showNewKey(created, modal, close) {
    const field = el("input", { type: "text", readonly: true, value: created.api_key, "aria-label": "New API key" });
    const copy = el("button", {
      class: "btn", type: "button", text: "Copy",
      onclick: async () => {
        try { await navigator.clipboard.writeText(created.api_key); copy.textContent = "Copied"; }
        catch (_) { field.select(); }
      },
    });
    modal.replaceChildren(
      el("h1", { text: "Key created for " + created.name }),
      el("p", { class: "sub", text: "Copy it now. The gateway stores only its hash and cannot show it again." }),
      el("div", { class: "secret" }, field, copy),
      el("div", { class: "actions" }, el("button", { class: "btn primary", type: "button", text: "Done", onclick: () => { close(); renderKeys(); } })));
    field.select();
  }

  // ---------- providers & routes ----------
  const KIND_LABEL = { openai: "OpenAI-compatible", anthropic: "Anthropic", azure: "Azure OpenAI", bedrock: "AWS Bedrock", vertex: "Google Vertex AI" };
  const SOURCE_LABEL = { config: "Config file", builtin: "Built in", dashboard: "Dashboard" };
  const STRATEGY_LABEL = { fallback: "In order", weighted: "Weighted split", latency: "Fastest first", cost: "Cheapest first" };

  async function renderSetup() {
    charts = [];
    const main = shell(el("div", { class: "empty", text: "Loading…" }));
    let c, provs, routesRes;
    try {
      [c, provs, routesRes] = await Promise.all([api("/api/config"), api("/api/providers"), api("/api/routes")]);
    } catch (e) { return handleError(e, main); }
    let pricing = null;
    try { pricing = await api("/api/pricing"); } catch (_) {}
    const health = Object.fromEntries(c.providers.map((p) => [p.name, p]));
    const storage = provs.storage_enabled;

    const keyText = (p) => ({
      stored: "Stored here " + p.key_hint,
      environment: "Environment " + p.key_hint,
      config: "Config file " + p.key_hint,
      cloud: "Cloud credentials",
      none: "No key",
    })[p.key];
    const provRow = (p) => {
      const h = health[p.name];
      const status = p.error ? el("span", { class: "pill err", title: p.error, text: "Not applied" })
        : !p.ready ? el("span", { class: "pill", text: "Needs a key" })
        : h && h.circuit_open ? el("span", { class: "pill err", text: "Circuit open" })
        : el("span", { class: "pill ok", text: "Ready" });
      return el("tr", {},
        el("td", {}, el("div", {}, el("strong", { text: p.name }), p.default ? el("span", { class: "pill", style: "margin-left:6px", text: "Default" }) : null),
          p.label ? el("div", { class: "sub", text: p.label }) : null),
        el("td", {}, el("div", { text: KIND_LABEL[p.kind] || p.kind || "–" }),
          p.error ? el("div", { class: "sub", style: "color:var(--error)", text: p.error })
            : el("div", { class: "sub trunc", title: p.base_url, text: p.base_url })),
        el("td", { text: keyText(p) }),
        el("td", {}, status),
        el("td", { text: SOURCE_LABEL[p.source] || p.source }),
        el("td", { class: "actions-cell" },
          storage && p.kind !== "bedrock" && p.kind !== "vertex"
            ? el("button", { class: "btn", type: "button", text: p.key === "none" ? "Add key" : "Replace key", onclick: () => setProviderKey(p.name) }) : null, " ",
          p.key === "stored" && p.source !== "dashboard"
            ? el("button", { class: "btn danger", type: "button", text: "Remove key", onclick: () => removeProviderKey(p.name) }) : null,
          p.source === "dashboard"
            ? el("button", { class: "btn danger", type: "button", text: "Remove", onclick: () => removeProvider(p.name) }) : null));
    };
    const provTable = (rows) => el("table", {},
      el("thead", {}, el("tr", {}, ["Provider", "Type and endpoint", "Key", "Status", "Defined in", ""].map((h) => el("th", { text: h })))),
      el("tbody", {}, rows.map(provRow)));
    const ready = provs.data.filter((p) => p.ready || p.error);
    const waiting = provs.data.filter((p) => !p.ready && !p.error);
    const providersPanel = el("section", { class: "panel flush" },
      el("div", { class: "page-head", style: "padding:12px 16px 0;margin:0" },
        el("div", {}, el("h2", { style: "padding:0", text: "Providers" }),
          el("div", { class: "sub", text: "Call any of them as provider/model. Built-in providers turn on when they have a key: from their environment variable, added here, or sent by the client." })),
        el("button", { class: "btn primary", type: "button", text: "Add provider", onclick: () => openProviderForm(storage) })),
      el("div", { class: "table-wrap" }, ready.length ? provTable(ready) : el("div", { class: "empty", text: "No provider has a key yet. Add one here or set its environment variable." })),
      waiting.length ? el("details", { class: "more" },
        el("summary", { text: waiting.length + " more built-in provider" + (waiting.length > 1 ? "s" : "") + " without a key" }),
        el("div", { class: "table-wrap" }, provTable(waiting))) : null,
      storage ? null : el("p", { class: "sub", style: "padding:0 16px 12px", text: "Set admin.encryption_key in the config to add provider keys here." }));

    const healthCards = c.providers.filter((p) => p.targets.length).map((p) =>
      el("div", { class: "prov" },
        el("div", { class: "name" }, el("span", { text: p.name }),
          el("span", { class: "pill " + (p.circuit_open ? "err" : "ok"), text: p.circuit_open ? "Circuit open" : "Healthy" })),
        el("div", { class: "meta", text: p.timeout_secs + " s timeout, " + p.max_retries + " retries. APIs: " + p.endpoints.join(", ") }),
        el("table", { class: "mini" },
          el("tbody", {}, p.targets.slice(0, 6).map((t) => el("tr", {},
            el("td", { class: "trunc", text: t.target }),
            el("td", { class: "num", text: t.ttft_ms ? ms(t.ttft_ms) : "–" }),
            el("td", { class: "num", title: "Output tokens per second", text: t.tokens_per_sec ? Math.round(t.tokens_per_sec) + " t/s" : "–" }),
            el("td", { class: "num", title: "Success rate", text: Math.round(t.success_rate * 100) + "%" })))))));

    const per = (v) => v === null || v === undefined ? "–" : "$" + (+v.toFixed(4));
    const prices = pricing ? el("section", { class: "panel flush", style: "margin-top:20px" },
      el("div", { class: "page-head", style: "padding:12px 16px 0;margin:0" },
        el("div", {}, el("h2", { style: "padding:0", text: "Model prices" }),
          el("div", { class: "sub", text: !pricing.enabled ? "Price list off. Costs come from prices set in the config." :
            pricing.models ? num(pricing.models) + " models in the price list, updated " + new Date(pricing.updated_at * 1000).toLocaleString() + "." :
            "The price list has not loaded yet." })),
        pricing.enabled ? el("button", { class: "btn", type: "button", text: "Update prices", onclick: refreshPricing }) : null),
      el("div", { class: "table-wrap" }, pricing.targets.length ? el("table", {},
        el("thead", {}, el("tr", {}, ["Route", "Target", "Input / M tokens", "Output / M tokens", "Cached input / M", "From"].map((h) => el("th", { text: h })))),
        el("tbody", {}, pricing.targets.map((t) => el("tr", {},
          el("td", { text: t.route }), el("td", { class: "trunc", text: t.target }),
          el("td", { class: "num", text: t.price ? per(t.price.input_per_mtok) : "–" }),
          el("td", { class: "num", text: t.price ? per(t.price.output_per_mtok) : "–" }),
          el("td", { class: "num", text: t.price ? per(t.price.cached_input_per_mtok) : "–" }),
          el("td", { text: ({ route: "Route config", override: "Override", list: "Price list" })[t.source] || "Unknown, no cost recorded" })))))
        : el("div", { class: "empty", text: "No routes to price." }))) : null;

    const routes = routesRes.data.length ? el("table", {},
      el("thead", {}, el("tr", {}, ["Route", "Strategy", "Targets, in fallback order", "Defined in", ""].map((h) => el("th", { text: h })))),
      el("tbody", {}, routesRes.data.map((r) => el("tr", {},
        el("td", {}, el("div", { text: r.name }), r.error ? el("div", { class: "sub", style: "color:var(--error)", text: "Not applied: " + r.error }) : null),
        el("td", { text: (STRATEGY_LABEL[r.strategy] || r.strategy || "In order") + ((r.branches || []).length ? ", " + r.branches.length + " conditional branch" + (r.branches.length > 1 ? "es" : "") : "") }),
        el("td", { class: "trunc", text: (r.targets || []).map((t) => t.provider + "/" + t.model + (r.strategy === "weighted" ? " (" + (t.weight === undefined ? 1 : t.weight) + ")" : "")).join("  ›  ") }),
        el("td", { text: SOURCE_LABEL[r.source] }),
        el("td", { class: "actions-cell" }, r.source === "dashboard" ? [
          r.error ? null : el("button", { class: "btn", type: "button", text: "Edit", onclick: () => openRouteForm(provs.data, r) }), " ",
          el("button", { class: "btn danger", type: "button", text: "Delete", onclick: () => removeRoute(r.name) }),
        ] : null)))))
      : el("div", { class: "empty", text: "No routes. Clients call provider/model directly." });
    const guards = c.guardrails.length ? el("table", {},
      el("thead", {}, el("tr", {}, ["Rule", "Action", "Checks"].map((h) => el("th", { text: h })))),
      el("tbody", {}, c.guardrails.map((g) => el("tr", {},
        el("td", { text: g.name }), el("td", { text: g.action }),
        el("td", { text: [g.request ? "prompts" : null, g.response ? "responses" : null].filter(Boolean).join(" and ") })))))
      : el("div", { class: "empty", text: "No guardrail rules. Add them under guardrails.rules in the config." });
    main.replaceChildren(
      el("div", { class: "page-head" }, el("div", {}, el("h1", { text: "Providers & routes" }),
        el("div", { class: "sub", text: "Version " + c.version + ", " + c.store + " store. Changes made here reach every replica within seconds." })),
        c.reloadable ? el("button", { class: "btn", type: "button", text: "Reload config file", onclick: reloadConfig }) : null),
      providersPanel,
      el("section", { class: "panel flush", style: "margin-top:20px" },
        el("div", { class: "page-head", style: "padding:12px 16px 0;margin:0" },
          el("div", {}, el("h2", { style: "padding:0", text: "Routes" }),
            el("div", { class: "sub", text: "A route is a model name of your own with fallbacks." + (c.default_provider ? " Any model that isn't a route or provider/model goes to " + c.default_provider + "." : "") })),
          el("button", { class: "btn primary", type: "button", text: "Add route", onclick: () => openRouteForm(provs.data, null) })),
        el("div", { class: "table-wrap" }, routes)),
      healthCards.length ? el("div", { style: "margin-top:20px" }, el("h2", { text: "Provider health on this replica" }), el("div", { class: "cards" }, healthCards)) : "",
      el("div", { class: "grid2", style: "margin-top:20px" },
        el("section", { class: "panel flush" }, el("h2", { text: "Guardrails" }), el("div", { class: "table-wrap" }, guards)),
        el("dl", { class: "kv" },
          el("dt", { text: "Response cache" }), el("dd", { text: c.cache.enabled ? "On, " + c.cache.ttl_secs + " s" : "Off" }),
          el("dt", { text: "Request log" }), el("dd", { text: c.logging.enabled ? "Last " + num(c.logging.max_entries) + " requests" : "Off" }),
          el("dt", { text: "Prompt capture" }), el("dd", { text: c.logging.capture_bodies ? "On" : "Off" }),
          el("dt", { text: "Log export" }), el("dd", { text: c.logging.export || "Off" }),
          el("dt", { text: "OpenTelemetry" }), el("dd", { text: c.logging.otlp || "Off" }),
          el("dt", { text: "Alerts webhook" }), el("dd", { text: c.alerts ? "On" : "Off" }))),
      prices || "");
  }

  function modalForm(title, fields, submitText, onSubmit) {
    const err = el("div", { class: "notice err hidden" });
    const close = () => { scrim.remove(); modal.remove(); };
    const scrim = el("div", { class: "scrim", onclick: close });
    const submit = el("button", { class: "btn primary", type: "submit", text: submitText });
    const form = el("form", {
      onsubmit: async (e) => {
        e.preventDefault();
        submit.disabled = true;
        try { await onSubmit(); close(); renderSetup(); }
        catch (x) { err.textContent = x.message; err.classList.remove("hidden"); }
        submit.disabled = false;
      },
    },
      el("h1", { text: title }), el("div", { class: "form-grid" }, fields), err,
      el("div", { class: "actions" }, el("button", { class: "btn", type: "button", text: "Cancel", onclick: close }), submit));
    const modal = el("div", { class: "modal", role: "dialog", "aria-label": title }, form);
    document.body.append(scrim, modal);
    const first = form.querySelector("input, select, textarea");
    if (first) first.focus();
  }

  function openProviderForm(storage) {
    const name = el("input", { type: "text", required: true, maxlength: "64", pattern: "[A-Za-z0-9._-]+", autocomplete: "off", placeholder: "my-vllm" });
    const kind = el("select", {}, Object.entries(KIND_LABEL).map(([v, l]) => el("option", { value: v, text: l })));
    const base = el("input", { type: "url", placeholder: "https://api.example.com/v1" });
    const key = el("input", { type: "password", autocomplete: "off", placeholder: storage ? "Stored encrypted" : "Needs admin.encryption_key" , disabled: !storage });
    const region = el("input", { type: "text", placeholder: "us-east-1 or us-central1" });
    const project = el("input", { type: "text", placeholder: "my-gcp-project" });
    const apiVersion = el("input", { type: "text", placeholder: "2024-10-21" });
    const zdr = el("input", { type: "checkbox" });
    const f = {
      base: el("label", { class: "field wide" }, "Base URL", base),
      key: el("label", { class: "field wide" }, "API key", key),
      region: el("label", { class: "field" }, "Region", region),
      project: el("label", { class: "field" }, "Google Cloud project", project),
      apiVersion: el("label", { class: "field" }, "API version", apiVersion),
    };
    const sync = () => {
      const k = kind.value;
      f.base.classList.toggle("hidden", k === "bedrock" || k === "vertex");
      base.required = !(k === "bedrock" || k === "vertex");
      f.region.classList.toggle("hidden", !(k === "bedrock" || k === "vertex"));
      f.project.classList.toggle("hidden", k !== "vertex");
      f.apiVersion.classList.toggle("hidden", k !== "azure");
      base.placeholder = k === "azure" ? "https://YOUR-RESOURCE.openai.azure.com/openai/deployments" : k === "anthropic" ? "https://api.anthropic.com/v1" : "https://api.example.com/v1";
    };
    kind.addEventListener("change", sync);
    sync();
    modalForm("Add a provider", [
      el("label", { class: "field" }, "Name, used as name/model", name),
      el("label", { class: "field" }, "Type", kind),
      f.base, f.key, f.region, f.project, f.apiVersion,
      el("label", { class: "toggle wide" }, zdr, "This provider keeps no data (zero data retention)"),
      el("p", { class: "sub wide", text: "Bedrock and Vertex use the gateway's AWS or Google credentials. A key entered here is encrypted and never shown again." }),
    ], "Add provider", async () => {
      const body = { kind: kind.value, zero_data_retention: zdr.checked || undefined };
      if (base.value.trim()) body.base_url = base.value.trim();
      if (key.value.trim()) body.api_key = key.value.trim();
      if (region.value.trim()) body.region = region.value.trim();
      if (project.value.trim()) body.project = project.value.trim();
      if (apiVersion.value.trim()) body.api_version = apiVersion.value.trim();
      await api("/api/providers/" + encodeURIComponent(name.value.trim()), { method: "PUT", body: JSON.stringify(body) });
    });
  }

  function openRouteForm(providers, existing) {
    const name = el("input", { type: "text", required: true, maxlength: "64", pattern: "[A-Za-z0-9._-]+", autocomplete: "off", placeholder: "assistant", value: existing ? existing.name : "", readonly: !!existing });
    const strategy = el("select", {}, Object.entries(STRATEGY_LABEL).map(([v, l]) => el("option", { value: v, text: l, selected: existing && existing.strategy === v })));
    const lines = existing ? existing.targets.map((t) => t.provider + "/" + t.model + (t.weight !== undefined ? " " + t.weight : "")).join("\n") : "";
    const targets = el("textarea", { rows: "5", required: true, placeholder: "openai/gpt-4o-mini\nanthropic/claude-haiku-4-5\nopenrouter/google/gemini-2.5-flash" });
    targets.value = lines;
    const names = providers.filter((p) => p.ready).map((p) => p.name).join(", ");
    modalForm(existing ? "Edit route " + existing.name : "Add a route", [
      el("label", { class: "field" }, "Route name, sent as model", name),
      el("label", { class: "field" }, "Strategy", strategy),
      el("label", { class: "field wide" }, "Targets, one per line as provider/model, tried in this order. For a weighted split add the weight after a space.", targets),
      el("p", { class: "sub wide", text: "Providers with a key: " + (names || "none yet") + "." }),
    ], existing ? "Save route" : "Add route", async () => {
      const parsed = targets.value.split("\n").map((l) => l.trim()).filter(Boolean).map((l) => {
        const [target, weight] = l.split(/\s+/);
        const i = target.indexOf("/");
        if (i < 1) throw new Error("“" + l + "” should look like provider/model.");
        const t = { provider: target.slice(0, i), model: target.slice(i + 1) };
        if (weight !== undefined) t.weight = Number(weight);
        return t;
      });
      if (!parsed.length) throw new Error("Add at least one target.");
      const body = { strategy: strategy.value, targets: parsed };
      if (existing && existing.branches && existing.branches.length) body.branches = existing.branches;
      await api("/api/routes/" + encodeURIComponent(name.value.trim()), { method: "PUT", body: JSON.stringify(body) });
    });
  }

  async function removeProvider(name) {
    if (!confirm("Remove the provider " + name + "? Requests for " + name + "/… then go to the default provider.")) return;
    try { await api("/api/providers/" + encodeURIComponent(name), { method: "DELETE" }); } catch (e) { alert(e.message); return; }
    renderSetup();
  }

  async function removeRoute(name) {
    if (!confirm("Delete the route " + name + "? Clients sending it as the model will get an error or reach the default provider.")) return;
    try { await api("/api/routes/" + encodeURIComponent(name), { method: "DELETE" }); } catch (e) { alert(e.message); return; }
    renderSetup();
  }

  async function reloadConfig() {
    try {
      const r = await api("/api/reload", { method: "POST" });
      alert("Config reloaded." + (r.notes.length ? "\n\n" + r.notes.join("\n") : ""));
    } catch (e) { alert(e.message); }
    renderSetup();
  }

  async function refreshPricing() {
    try { await api("/api/pricing/refresh", { method: "POST" }); } catch (e) { alert(e.message); return; }
    renderSetup();
  }

  async function setProviderKey(provider) {
    const key = prompt("API key for " + provider + ". It is encrypted and never shown again.");
    if (!key) return;
    try { await api("/api/provider-keys/" + encodeURIComponent(provider), { method: "PUT", body: JSON.stringify({ api_key: key }) }); }
    catch (e) { alert(e.message); return; }
    renderSetup();
  }

  async function removeProviderKey(provider) {
    if (!confirm("Remove the stored key for " + provider + "? Its key from the config file or environment is used again, if it has one.")) return;
    try { await api("/api/provider-keys/" + encodeURIComponent(provider), { method: "DELETE" }); } catch (e) { alert(e.message); return; }
    renderSetup();
  }

  // ---------- models ----------
  async function renderModels(force) {
    charts = [];
    const main = shell(el("div", { class: "empty", text: force ? "Asking every provider for its models…" : "Loading…" }));
    let res;
    try { res = await api(force ? "/api/models/refresh" : "/api/models", force ? { method: "POST" } : undefined); }
    catch (e) { return handleError(e, main); }
    const search = el("input", { type: "search", placeholder: "Search models", "aria-label": "Search models", value: state.modelSearch || "" });
    const provider = el("select", { "aria-label": "Provider" }, el("option", { value: "", text: "All providers" }),
      res.providers.map((p) => el("option", { value: p.provider, selected: state.modelProvider === p.provider, text: p.provider + " (" + num(p.models) + ")" })));
    const per = (v) => v === null || v === undefined ? "–" : "$" + (+v.toFixed(4));
    const host = el("div");
    const LIMIT = 500;
    const draw = () => {
      state.modelSearch = search.value; state.modelProvider = provider.value;
      const q = search.value.trim().toLowerCase();
      const rows = res.data.filter((m) => (!provider.value || m.owned_by === provider.value)
        && (!q || m.id.toLowerCase().includes(q) || (m.name || "").toLowerCase().includes(q)));
      host.replaceChildren(
        el("section", { class: "panel flush" }, el("div", { class: "table-wrap" }, rows.length ? el("table", {},
          el("thead", {}, el("tr", {}, ["Model, as clients send it", "Name", "Context", "Input / M", "Output / M", "Price from"].map((h, i) => el("th", { class: i >= 2 && i <= 4 ? "num" : null, text: h })))),
          el("tbody", {}, rows.slice(0, LIMIT).map((m) => {
            const copy = el("button", { class: "link", type: "button", title: "Copy model name", text: m.id,
              onclick: async () => { try { await navigator.clipboard.writeText(m.id); copy.textContent = "Copied"; setTimeout(() => { copy.textContent = m.id; }, 900); } catch (_) {} } });
            return el("tr", {},
              el("td", {}, copy),
              el("td", { class: "trunc", text: m.name || "–" }),
              el("td", { class: "num", text: m.context_length ? compact(m.context_length) : "–" }),
              el("td", { class: "num", text: per(m.pricing.input_per_mtok) }),
              el("td", { class: "num", text: per(m.pricing.output_per_mtok) }),
              el("td", { text: ({ list: "Price list", provider: "Provider", override: "Override", route: "Route config" })[m.pricing.source] || "Unknown" }));
          })))
          : el("div", { class: "empty", text: res.data.length ? "No model matches." : "No provider with a model list has a key yet. Add one on the Providers & routes page." }))),
        rows.length > LIMIT ? el("p", { class: "sub", text: "Showing " + LIMIT + " of " + num(rows.length) + ". Search to narrow the list." }) : "");
    };
    search.addEventListener("input", draw);
    provider.addEventListener("change", draw);
    const problems = res.providers.filter((p) => p.error);
    main.replaceChildren(
      el("div", { class: "page-head" }, el("div", {}, el("h1", { text: "Models" }),
        el("div", { class: "sub", text: num(res.data.length) + " models from " + res.providers.filter((p) => p.models).length + " providers, from each provider's own list. Clients get the models their key may use from GET /v1/models." })),
        el("button", { class: "btn", type: "button", text: "Refresh lists", onclick: () => renderModels(true) })),
      problems.length ? el("div", { class: "notice err" }, problems.map((p) => el("div", { text: p.provider + ": " + p.error + (p.models ? " (showing the last list)" : "") }))) : "",
      el("div", { class: "filters" }, search, provider),
      host);
    draw();
  }

  // ---------- routing rules ----------
  async function renderRules() {
    charts = [];
    const main = shell(el("div", { class: "empty", text: "Loading…" }));
    let res;
    try { res = await api("/api/rules"); } catch (e) { return handleError(e, main); }
    const source = el("input", { type: "text", placeholder: "openai/gpt-4o or openai/*", required: true });
    const action = el("select", {}, el("option", { value: "rewrite", text: "Rewrite to" }), el("option", { value: "deny", text: "Deny" }));
    const dest = el("input", { type: "text", placeholder: "anthropic/claude-haiku-4.5" });
    const reason = el("input", { type: "text", placeholder: "Reason shown to clients (optional)" });
    action.addEventListener("change", () => { dest.disabled = action.value === "deny"; });
    const err = el("div", { class: "notice err hidden" });
    const form = el("form", {
      class: "filters",
      onsubmit: async (e) => {
        e.preventDefault();
        const body = { source: source.value.trim(), action: action.value, reason: reason.value.trim() || undefined };
        if (action.value === "rewrite") body.destination = dest.value.trim();
        try { await api("/api/rules", { method: "POST", body: JSON.stringify(body) }); renderRules(); }
        catch (x) { err.textContent = x.message; err.classList.remove("hidden"); }
      },
    }, source, action, dest, reason, el("button", { class: "btn primary", type: "submit", text: "Add rule" }));
    const table = res.data.length ? el("table", {},
      el("thead", {}, el("tr", {}, ["Model", "Action", "Reason", "Defined in", ""].map((h) => el("th", { text: h })))),
      el("tbody", {}, res.data.map((r) => el("tr", {},
        el("td", { text: r.source }),
        el("td", { text: r.action === "deny" ? "Deny" : "Rewrite to " + r.destination }),
        el("td", { class: "trunc", text: r.reason || "–" }),
        el("td", { text: r.origin === "admin" ? "Dashboard" : "Config file" }),
        el("td", {}, r.origin === "admin" ? [
          el("button", { class: "btn", type: "button", text: r.enabled ? "Turn off" : "Turn on", onclick: async () => { try { await api("/api/rules/" + r.id, { method: "PATCH", body: JSON.stringify({ enabled: !r.enabled }) }); } catch (e) { alert(e.message); } renderRules(); } }), " ",
          el("button", { class: "btn danger", type: "button", text: "Delete", onclick: async () => { if (!confirm("Delete this rule?")) return; try { await api("/api/rules/" + r.id, { method: "DELETE" }); } catch (e) { alert(e.message); } renderRules(); } }),
        ] : null)))))
      : el("div", { class: "empty", text: "No rules. Requests go to the model they ask for." });
    main.replaceChildren(
      el("div", { class: "page-head" }, el("div", {}, el("h1", { text: "Routing rules" }),
        el("div", { class: "sub", text: "Send requests for one model to another, or block a model. Rules apply to fallbacks too and reach every replica within seconds." }))),
      form, err, el("section", { class: "panel flush" }, el("div", { class: "table-wrap" }, table)));
  }

  // ---------- limits ----------
  async function renderLimits() {
    charts = [];
    const main = shell(el("div", { class: "empty", text: "Loading…" }));
    let res;
    try { res = await api("/api/limits"); } catch (e) { return handleError(e, main); }
    const fmtVal = (kind, v) => v === null || v === undefined ? "–" : kind === "cost" ? usd(v) : compact(v);
    const table = res.data.length ? el("table", {},
      el("thead", {}, el("tr", {}, ["Limit", "Counts", "Per", "Scope", "Used this " + "window", "Resets"].map((h) => el("th", { text: h })))),
      el("tbody", {}, res.data.map((l) => {
        let used = el("span", { text: "Tracked per " + l.per });
        if (l.used !== null && l.used !== undefined) {
          const ratio = Math.min(1, l.used / l.limit);
          const b = el("span");
          b.style.width = Math.max(2, ratio * 100) + "%";
          used = el("div", {}, fmtVal(l.kind, l.used) + " of " + fmtVal(l.kind, l.limit), el("div", { class: "bar" + (ratio > 0.8 ? " warn" : "") }, b));
        }
        return el("tr", {},
          el("td", { text: l.name }), el("td", { text: l.kind + " per " + l.window }), el("td", { text: l.per }),
          el("td", { text: l.scope === "*" ? "Everything" : l.scope || "Each " + l.per }),
          el("td", {}, used), el("td", { text: new Date(l.resets_at * 1000).toLocaleString() }));
      })))
      : el("div", { class: "empty", text: "No limits configured. Add them under limits in the config file." });
    main.replaceChildren(
      el("div", { class: "page-head" }, el("div", {}, el("h1", { text: "Limits" }),
        el("div", { class: "sub", text: "Request, token and spend limits from the config file. Over a limit, requests get 429 until the window resets." }))),
      el("section", { class: "panel flush" }, el("div", { class: "table-wrap" }, table)));
  }

  // ---------- reports ----------
  async function renderReports() {
    charts = [];
    const today = new Date().toISOString().slice(0, 10);
    const monthAgo = new Date(Date.now() - 29 * 86400000).toISOString().slice(0, 10);
    const start = el("input", { type: "date", value: state.reportStart || monthAgo, "aria-label": "Start date" });
    const end = el("input", { type: "date", value: state.reportEnd || today, "aria-label": "End date" });
    const group = el("select", { "aria-label": "Group by" },
      [["day", "Day"], ["model", "Model"], ["provider", "Provider"], ["key", "API key"], ["user", "User"], ["tag", "Tag"], ["credential_type", "Provider key source"], ["zero_data_retention", "ZDR"]]
        .map(([v, l]) => el("option", { value: v, selected: (state.reportGroup || "day") === v, text: "By " + l.toLowerCase() })));
    const host = el("div", {}, el("div", { class: "empty", text: "Loading…" }));
    let rows = [];
    const load = async () => {
      Object.assign(state, { reportStart: start.value, reportEnd: end.value, reportGroup: group.value });
      const qs = new URLSearchParams({ start_date: start.value, end_date: end.value, group_by: group.value });
      let res;
      try { res = await api("/api/report?" + qs); } catch (e) { return handleError(e, host); }
      rows = res.results;
      const label = group.value === "key" ? "api_key_name" : group.value;
      if (!rows.length) { host.replaceChildren(el("div", { class: "panel empty", text: "No usage in this range." })); return; }
      host.replaceChildren(el("section", { class: "panel flush" }, el("div", { class: "table-wrap" }, el("table", {},
        el("thead", {}, el("tr", {}, [group.selectedOptions[0].text.replace("By ", ""), "Requests", "Failed", "Input tokens", "Cached", "Output tokens", "Reasoning", "Spend"].map((h, i) => el("th", { class: i ? "num" : null, text: h })))),
        el("tbody", {}, rows.map((r) => el("tr", {},
          el("td", { text: r[label] || "(none)" }), el("td", { class: "num", text: num(r.request_count) }), el("td", { class: "num", text: num(r.error_count) }),
          el("td", { class: "num", text: compact(r.input_tokens) }), el("td", { class: "num", text: compact(r.cached_input_tokens) }),
          el("td", { class: "num", text: compact(r.output_tokens) }), el("td", { class: "num", text: compact(r.reasoning_tokens) }),
          el("td", { class: "num", text: usd(r.total_cost) }))))))));
    };
    const csv = () => {
      if (!rows.length) return;
      const cols = Object.keys(rows[0]);
      const esc = (v) => '"' + String(v === null || v === undefined ? "" : v).replace(/"/g, '""') + '"';
      const text = [cols.join(",")].concat(rows.map((r) => cols.map((c) => esc(r[c])).join(","))).join("\n");
      const a = el("a", { href: URL.createObjectURL(new Blob([text], { type: "text/csv" })), download: "ai-gateway-report-" + start.value + "-to-" + end.value + ".csv" });
      document.body.append(a); a.click(); a.remove();
    };
    for (const i of [start, end, group]) i.addEventListener("change", load);
    shell([
      el("div", { class: "page-head" }, el("div", {}, el("h1", { text: "Usage reports" }),
        el("div", { class: "sub", text: "Spend and tokens by day, model, provider, key, user or tag (UTC dates). Clients can get their own report from GET /v1/report." })),
        el("div", { class: "filters" }, start, end, group, el("button", { class: "btn", type: "button", text: "Download CSV", onclick: csv }))),
      host,
    ]);
    load();
  }

  // ---------- audit ----------
  async function renderAudit() {
    charts = [];
    const main = shell(el("div", { class: "empty", text: "Loading…" }));
    let res;
    try { res = await api("/api/audit"); } catch (e) { return handleError(e, main); }
    const table = res.data.length ? el("table", {},
      el("thead", {}, el("tr", {}, ["Time", "Action", "Target", "Detail"].map((h) => el("th", { text: h })))),
      el("tbody", {}, res.data.map((a) => el("tr", {},
        el("td", { text: when(a.ts_ms) }), el("td", { text: a.action }), el("td", { text: a.target }),
        el("td", { class: "trunc", title: JSON.stringify(a.detail), text: JSON.stringify(a.detail) })))))
      : el("div", { class: "empty", text: "No admin changes yet." });
    main.replaceChildren(
      el("div", { class: "page-head" }, el("div", {}, el("h1", { text: "Audit log" }),
        el("div", { class: "sub", text: "Changes made through the dashboard or admin API: keys, rules, provider keys, log deletion and reloads." }))),
      el("section", { class: "panel flush" }, el("div", { class: "table-wrap" }, table)));
  }

  // ---------- router ----------
  function route() {
    stopLive();
    const page = (location.hash || "#overview").slice(1);
    state.page = PAGES.some(([id]) => id === page) ? page : "overview";
    if (!state.token) { renderLogin(); return; }
    ({ overview: renderOverview, logs: renderLogs, models: () => renderModels(false), reports: renderReports, keys: renderKeys, rules: renderRules, limits: renderLimits, setup: renderSetup, audit: renderAudit })[state.page]();
  }
  window.addEventListener("hashchange", route);
  route();
})();
