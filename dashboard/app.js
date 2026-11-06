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
    logs: { offset: 0, rows: [], total: 0, live: false, timer: null, filters: { q: "", status: "", tenant: "", model: "" } },
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
    for (const c of children.flat()) {
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
    ["keys", "API keys"],
    ["setup", "Providers & routes"],
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
    const pad = { l: 46, r: 8, t: 8, b: 22 };
    const iw = w - pad.l - pad.r, ih = h - pad.t - pad.b;
    const stacked = opts.kind === "bars";
    const totals = data.map((d) => stacked
      ? series.reduce((a, s) => a + (d[s.key] || 0), 0)
      : Math.max(...series.map((s) => d[s.key] || 0)));
    const max = niceMax(Math.max(0, ...totals));
    const fmt = opts.format || compact;
    ctx.clearRect(0, 0, w, h);
    ctx.font = "11px system-ui, sans-serif";
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
        fig("Guardrail hits", num(t.guardrail_hits), "Requests with a matched rule")));
    requestAnimationFrame(() => drawChart(canvas, series, trafficSeries, { kind: "bars", step: d.step_secs, height: 200 }));
    charts.push(() => drawChart(canvas, series, trafficSeries, { kind: "bars", step: d.step_secs, height: 200 }));

    const charts2 = el("div", { class: "grid2" },
      chartPanel("Tokens", d.series, [{ key: "tokens", color: "--tokens", label: "Tokens" }], { kind: "line", step: d.step_secs }),
      chartPanel("Spend", d.series, [{ key: "cost_usd", color: "--cost", label: "Spend" }], { kind: "bars", step: d.step_secs, format: usd }),
      chartPanel("Average latency", d.series, [{ key: "avg_latency_ms", color: "--traffic", label: "Latency" }], { kind: "line", step: d.step_secs, format: ms }),
      breakdownPanel("Status codes", d.statuses, [], t.requests));

    const tables = el("div", { class: "grid2" },
      breakdownPanel("Models", d.models, [["Tokens", "tokens", compact], ["Spend", "cost_usd", usd], ["Failed", "errors", num]], t.requests),
      breakdownPanel("API keys", d.keys, [["Tokens", "tokens", compact], ["Spend", "cost_usd", usd]], t.requests),
      breakdownPanel("Providers", d.providers.map((p) => ({ ...p, avg: p.requests ? Math.round(p.latency_sum_ms / p.requests) : 0 })),
        [["Failed", "errors", num], ["Avg latency", "avg", ms]], t.requests));

    main.replaceChildren(head, strip, charts2, tables);
    if (!t.requests) {
      main.insertBefore(el("div", { class: "notice", text: "No requests in this range yet. Send one to /v1/chat/completions and it will show up here." }), strip);
    }
  }

  function fig(label, value, detail, bad) {
    return el("div", { class: "fig" + (bad ? " bad" : "") },
      el("div", { class: "k", text: label }), el("div", { class: "v", text: value }), el("div", { class: "d", text: detail }));
  }

  function breakdownPanel(title, rows, cols, total) {
    if (!rows.length) return el("section", { class: "panel" }, el("h2", { text: title }), el("div", { class: "empty", text: "Nothing yet." }));
    const max = Math.max(...rows.map((r) => r.requests));
    return el("section", { class: "panel flush" }, el("h2", { text: title }),
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
      [["", "All statuses"], ["ok", "Succeeded"], ["error", "Failed"]].map(([v, l]) =>
        el("option", { value: v, selected: f.status === v, text: l })));
    const tenant = el("select", { "aria-label": "API key" }, el("option", { value: "", text: "All keys" }));
    const model = el("input", { type: "text", placeholder: "Model or route", value: f.model, "aria-label": "Model" });
    const live = el("input", { type: "checkbox", checked: state.logs.live });
    const apply = () => {
      Object.assign(f, { q: search.value, status: status.value, tenant: tenant.value, model: model.value });
      state.logs.offset = 0;
      loadLogs(false);
    };
    let debounce;
    search.addEventListener("input", () => { clearTimeout(debounce); debounce = setTimeout(apply, 250); });
    model.addEventListener("input", () => { clearTimeout(debounce); debounce = setTimeout(apply, 250); });
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
        el("label", { class: "toggle" }, live, "Update every 5 seconds")),
      el("div", { class: "filters" }, search, status, tenant, model),
      tableHost,
    ]);

    api("/api/keys").then((k) => {
      state.keys = k.data;
      for (const key of k.data) tenant.append(el("option", { value: key.name, selected: f.tenant === key.name, text: key.name }));
    }).catch(() => {});
    await loadLogs(false);
    void main;
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
          r.endpoint === "embeddings" ? el("span", { class: "pill", text: "embed" }) : null, " ",
          (r.guardrails || []).length ? el("span", { class: "pill flag", title: r.guardrails.join(", "), text: "guardrail" }) : null)))));
    const more = state.logs.rows.length < state.logs.total
      ? el("div", { class: "actions" }, el("button", { class: "btn", type: "button", text: "Show more", onclick: () => loadLogs(true) }))
      : null;
    host.replaceChildren(el("section", { class: "panel flush" }, el("div", { class: "table-wrap" }, table)), more);
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
    const rows = [
      ["Request id", r.id], ["Time", new Date(r.ts_ms).toLocaleString()], ["API key", r.tenant],
      ["Endpoint", r.endpoint], ["Requested model", r.route || "–"], ["Served by", (r.provider || "–") + " / " + (r.model || "–")],
      ["Status", String(r.status)], ["Attempts", String(r.attempts)], ["Streamed", r.stream ? "Yes" : "No"],
      ["Cache", r.cache_hit ? "Served from cache" : "Not cached"],
      ["Tokens", num(r.prompt_tokens) + " in, " + num(r.completion_tokens) + " out"],
      ["Spend", r.cost_usd ? usd(r.cost_usd) : "–"], ["Latency", ms(r.latency_ms)],
      ["Guardrails", (r.guardrails || []).join(", ") || "None matched"],
      ["Error", r.error || "None"],
    ];
    const closeBtn = el("button", { class: "btn", type: "button", text: "Close", onclick: close });
    const drawer = el("aside", { class: "drawer", role: "dialog", "aria-label": "Request detail" },
      el("header", {}, el("div", {}, el("h1", { text: "Request" }), el("div", { class: "sub", text: r.id })), closeBtn),
      el("dl", { class: "kv" }, rows.map(([k, v]) => [el("dt", { text: k }), el("dd", { text: v })])),
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
          el("td", { text: k.source === "admin" ? "Dashboard" : "Config file" }),
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
        el("label", { class: "field" }, "Concurrent requests per replica", conc)),
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
  async function renderSetup() {
    charts = [];
    const main = shell(el("div", { class: "empty", text: "Loading…" }));
    let c;
    try { c = await api("/api/config"); } catch (e) { return handleError(e, main); }
    const providers = el("div", { class: "cards" }, c.providers.map((p) =>
      el("div", { class: "prov" },
        el("div", { class: "name" }, el("span", { text: p.name }),
          el("span", { class: "pill " + (p.circuit_open ? "err" : "ok"), text: p.circuit_open ? "Circuit open" : "Healthy" })),
        el("div", { class: "meta", text: p.kind + " at " + p.base_url }),
        el("div", { class: "meta", text: p.timeout_secs + " s timeout, " + p.max_retries + " retries" + (p.embeddings ? ", embeddings" : "") }))));
    const routes = c.routes.length ? el("table", {},
      el("thead", {}, el("tr", {}, ["Route", "Strategy", "Targets, in fallback order"].map((h) => el("th", { text: h })))),
      el("tbody", {}, c.routes.map((r) => el("tr", {},
        el("td", { text: r.name }),
        el("td", { text: r.strategy === "weighted" ? "Weighted split" : "First healthy" }),
        el("td", { class: "trunc", text: r.targets.map((t) => t.provider + "/" + t.model + (r.strategy === "weighted" ? " (" + (t.weight === null ? 1 : t.weight) + ")" : "")).join("  ›  ") })))))
      : el("div", { class: "empty", text: "No routes. Clients call provider/model directly." });
    const guards = c.guardrails.length ? el("table", {},
      el("thead", {}, el("tr", {}, ["Rule", "Action", "Checks"].map((h) => el("th", { text: h })))),
      el("tbody", {}, c.guardrails.map((g) => el("tr", {},
        el("td", { text: g.name }), el("td", { text: g.action }),
        el("td", { text: [g.request ? "prompts" : null, g.response ? "responses" : null].filter(Boolean).join(" and ") })))))
      : el("div", { class: "empty", text: "No guardrail rules. Add them under guardrails.rules in the config." });
    main.replaceChildren(
      el("div", { class: "page-head" }, el("div", {}, el("h1", { text: "Providers & routes" }),
        el("div", { class: "sub", text: "Version " + c.version + ", " + c.store + " store. Defined in the config file." }))),
      el("h2", { text: "Providers" }), providers,
      el("div", { class: "grid2" },
        el("section", { class: "panel flush" }, el("h2", { text: "Routes" }), el("div", { class: "table-wrap" }, routes),
          c.default_provider ? el("p", { class: "sub", style: "padding: 0 16px 12px", text: "Any other model goes to " + c.default_provider + "." }) : null),
        el("section", { class: "panel flush" }, el("h2", { text: "Guardrails" }), el("div", { class: "table-wrap" }, guards))),
      el("dl", { class: "kv", style: "margin-top: 20px" },
        el("dt", { text: "Response cache" }), el("dd", { text: c.cache.enabled ? "On, " + c.cache.ttl_secs + " s" : "Off" }),
        el("dt", { text: "Request log" }), el("dd", { text: c.logging.enabled ? "Last " + num(c.logging.max_entries) + " requests" : "Off" }),
        el("dt", { text: "Prompt capture" }), el("dd", { text: c.logging.capture_bodies ? "On" : "Off" })));
  }

  // ---------- router ----------
  function route() {
    stopLive();
    const page = (location.hash || "#overview").slice(1);
    state.page = PAGES.some(([id]) => id === page) ? page : "overview";
    if (!state.token) { renderLogin(); return; }
    ({ overview: renderOverview, logs: renderLogs, keys: renderKeys, setup: renderSetup })[state.page]();
  }
  window.addEventListener("hashchange", route);
  route();
})();
