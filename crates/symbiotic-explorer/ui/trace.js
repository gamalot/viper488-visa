// The I/O trace window (trace.js, trace.html): every program's VISA calls on this computer, as `symbiotic-ctl trace
// show -f` prints them, in a table that follows new ones. The main window opens it from Settings (`open_trace` in
// src/main.rs). Written to its own elements, not rebuilt on every record: rows are appended as they come, and the whole
// table is rebuilt only when the filter changes.
"use strict";

const KEEP = 20000;      // the records this window holds; Export writes every one in the files
const POLL_MS = 500;
let records = [];        // as trace_read gives them, oldest first
const programs = new Map(); // pid -> the program's path
let state = { on: false, dir: "" };
let paused = false;
let filter = { text: stored("trace.filter", ""), nested: stored("trace.nested", "") === "1", errors: false };
let note = null;         // {text, error}
let ui = null;

const pad = (n, w) => String(n).padStart(w, "0");
function timeOf(r) {
  const d = new Date(Math.floor(r.at_ms));
  const us = Math.round((r.at_ms % 1000) * 1000) % 1000000;
  return `${pad(d.getHours(), 2)}:${pad(d.getMinutes(), 2)}:${pad(d.getSeconds(), 2)}.${pad(us, 6)}`;
}
function programOf(pid) {
  const path = programs.get(pid) || "";
  return { name: path.split(/[\\/]/).pop() || "?", path };
}
function statusOf(r) {
  return r.status_name || "0x" + (r.status >>> 0).toString(16).toUpperCase().padStart(8, "0");
}
function msOf(r) {
  return (r.dur_us / 1000).toFixed(3);
}
function dataOf(r) {
  return r.data + (r.more ? "…" : "");
}

function shown(r) {
  if (!filter.nested && r.depth > 1) return false;
  if (filter.errors && r.status >= 0) return false;
  const q = filter.text.trim().toLowerCase();
  if (!q) return true;
  const p = programOf(r.pid);
  return [p.name, String(r.pid), r.op, r.resource, statusOf(r), r.data].some(x => x.toLowerCase().includes(q));
}

function rowEl(r) {
  const p = programOf(r.pid);
  return el("tr", { class: r.status < 0 ? "error" : r.depth > 1 ? "nested" : null },
    el("td", { class: "mono time" }, timeOf(r)),
    el("td", { title: p.path }, `${p.name} `, el("span", { class: "muted" }, String(r.pid))),
    el("td", { class: "mono", style: `padding-left: ${(r.depth - 1) * 16 + 8}px` }, r.op),
    el("td", { class: "mono", title: r.vi ? `vi ${r.vi}` : null }, r.resource),
    el("td", { class: "mono" }, statusOf(r)),
    el("td", { class: "mono num" }, msOf(r)),
    el("td", { class: "mono num" }, r.count === null || r.count === undefined ? "" : String(r.count)),
    el("td", { class: "mono data", title: r.data ? dataOf(r) : null }, dataOf(r)));
}

function emptyText() {
  return t(state.on ? "trace.empty_on" : "trace.empty_off");
}

// Whether the table is scrolled to its end: new rows keep it there; scrolled up, it stays where the user put it.
function atEnd() {
  const s = ui.scroller;
  return s.scrollHeight - s.scrollTop - s.clientHeight < 24;
}

function rebuild() {
  const rows = records.filter(shown);
  ui.body.replaceChildren(...rows.map(rowEl));
  ui.empty.textContent = rows.length ? "" : records.length ? t("trace.none_shown") : emptyText();
  ui.empty.hidden = rows.length > 0;
  ui.scroller.scrollTop = ui.scroller.scrollHeight;
  counts();
}

function counts() {
  const n = records.length, s = ui.body.childElementCount;
  ui.count.textContent = s === n ? t("trace.count", { n }) : t("trace.count_shown", { n, shown: s });
  ui.copy.disabled = s === 0;
}

function append(news) {
  if (!news.length) return;
  const follow = atEnd();
  records.push(...news);
  const drop = records.length - KEEP;
  if (drop > 0) {
    // The oldest go, from the table too.
    const gone = records.splice(0, drop).filter(shown).length;
    for (let i = 0; i < gone && ui.body.firstChild; i++) ui.body.firstChild.remove();
  }
  const rows = news.filter(shown).map(rowEl);
  if (rows.length) {
    ui.body.append(...rows);
    ui.empty.hidden = true;
  }
  if (follow) ui.scroller.scrollTop = ui.scroller.scrollHeight;
  counts();
}

async function poll(fromStart) {
  if (paused && !fromStart) return;
  try {
    const n = await invoke("trace_read", { fromStart: !!fromStart });
    for (const [pid, path] of Object.entries(n.programs || {})) programs.set(Number(pid), path);
    if (fromStart) {
      records = n.records.slice(-KEEP);
      rebuild();
    } else {
      append(n.records);
    }
  } catch (e) { say(String(e), true); }
}

async function readState() {
  try { state = await invoke("trace_state"); } catch (e) { say(String(e), true); }
  head();
}

function say(text, error) {
  note = { text, error };
  if (!ui) return;
  ui.note.textContent = text;
  ui.note.className = error ? "error" : "muted";
}

// The parts that change with the switch.
function head() {
  if (!ui) return;
  ui.on.checked = state.on;
  ui.dot.className = "dot " + (state.on ? "ok" : "idle");
  ui.stateText.textContent = t(state.on ? "trace.state_on" : "trace.state_off");
  ui.hint.textContent = t("trace.hint", { dir: state.dir });
  if (!records.length) ui.empty.textContent = emptyText();
}

async function setOn(on) {
  try {
    state = await invoke("trace_set", { on });
    if (on) {
      records = [];
      rebuild();
    }
    say(t(on ? "trace.started" : "trace.stopped"), false);
  } catch (e) { say(String(e), true); }
  head();
}

async function clearAll() {
  try {
    await invoke("trace_clear");
    records = [];
    rebuild();
    say(t("trace.cleared"), false);
  } catch (e) { say(String(e), true); }
}

async function exportAll() {
  try {
    const path = await invoke("trace_export", { kind: t("trace.text_kind") });
    if (path) say(t("trace.exported", { path }), false);
  }
  catch (e) { say(String(e), true); }
}

async function copyShown() {
  const rows = records.filter(shown);
  const text = rows.map(r => [timeOf(r), `${programOf(r.pid).name} ${r.pid}`, "  ".repeat(r.depth - 1) + r.op, r.resource, statusOf(r), msOf(r), r.count ?? "", dataOf(r)].join("\t")).join("\n");
  try {
    await navigator.clipboard.writeText(text + "\n");
    say(t("trace.copied", { n: rows.length }), false);
  } catch (e) { say(String(e), true); }
}

function build() {
  const on = el("input", { type: "checkbox" });
  on.addEventListener("change", () => setOn(on.checked));
  const q = el("input", { type: "text", class: "grow", spellcheck: "false", autocomplete: "off", placeholder: t("trace.filter_placeholder"), "aria-label": t("trace.filter_placeholder") });
  q.value = filter.text;
  q.addEventListener("input", () => { filter.text = q.value; store("trace.filter", q.value); rebuild(); });
  const nested = el("input", { type: "checkbox", checked: filter.nested });
  nested.addEventListener("change", () => { filter.nested = nested.checked; store("trace.nested", nested.checked ? "1" : ""); rebuild(); });
  const errors = el("input", { type: "checkbox", checked: filter.errors });
  errors.addEventListener("change", () => { filter.errors = errors.checked; rebuild(); });
  const pause = el("button", { type: "button" }, t("trace.pause"));
  pause.addEventListener("click", () => {
    paused = !paused;
    pause.textContent = t(paused ? "trace.resume" : "trace.pause");
    pause.classList.toggle("primary", paused);
    if (!paused) poll(false);
  });
  const copy = el("button", { type: "button", onclick: copyShown }, t("trace.copy"));
  const dot = el("span", { class: "dot idle" });
  const stateText = el("span", { class: "muted" });
  const hint = el("div", { class: "hint" });
  const noteEl = el("span", { role: "status", class: note && note.error ? "error" : "muted" }, note ? note.text : "");
  const count = el("span", { class: "muted" });
  const body = el("tbody");
  const empty = el("div", { class: "hint trace-empty" });
  const cols = ["time", "program", "call", "resource", "status", "ms", "bytes", "data"];
  const scroller = el("div", { class: "trace-table" },
    el("table", {},
      el("thead", {}, el("tr", {}, cols.map(c => el("th", { class: c === "ms" || c === "bytes" ? "num" : null }, t("trace.col." + c))))),
      body),
    empty);
  document.getElementById("trace").replaceChildren(
    el("div", { class: "console-head" },
      el("h1", {}, t("trace.title")),
      el("label", { class: "switch inline", title: t("trace.on_hint") }, on, el("span", {}, t("trace.on"))),
      dot, stateText,
      el("span", { class: "spacer" }),
      pause,
      el("button", { type: "button", onclick: clearAll }, t("trace.clear")),
      copy,
      el("button", { type: "button", onclick: exportAll, title: t("trace.export_hint") }, t("trace.export"))),
    hint,
    el("div", { class: "row" }, q,
      el("label", { class: "switch inline", title: t("trace.nested_hint") }, nested, el("span", {}, t("trace.nested"))),
      el("label", { class: "switch inline" }, errors, el("span", {}, t("trace.errors")))),
    scroller,
    el("div", { class: "row" }, count, el("span", { class: "spacer" }), noteEl));
  ui = { on, dot, stateText, hint, note: noteEl, count, body, empty, scroller, copy };
}

async function main() {
  await startWindow();
  hooks.say = say;
  build();
  setWindowTitle(t("trace.title"));
  document.title = t("trace.title");
  await readState();
  await poll(true);
  setInterval(() => { if (!document.hidden) poll(false); }, POLL_MS);
  // The switch may be turned by `symbiotic-ctl trace` or the main window's Settings.
  setInterval(() => { if (!document.hidden) readState(); }, 2000);

  followChanges(() => { build(); head(); rebuild(); });
}

// ---- the browser's own keys and menu: none of them has a place here (Reload would drop what is shown) -------
// As console.js: zoom keys zoom; Copy stays (a selection of rows), the rest is kept from the webview.

document.addEventListener("contextmenu", e => {
  if (e.target.closest && e.target.closest('input[type="text"]')) return; // cut, copy, paste
  e.preventDefault();
});
window.addEventListener("mouseup", e => { if (e.button === 3 || e.button === 4) e.preventDefault(); });
document.addEventListener("keydown", e => {
  const k = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  const mod = e.ctrlKey || e.metaKey;
  if (mod && !e.altKey && ["=", "+", "-", "_", "0"].includes(k)) {
    e.preventDefault();
    if (k === "0") setZoom(100, false); else zoomStep(k === "-" || k === "_" ? -1 : 1);
    return;
  }
  if (k === "F5" || k === "BrowserRefresh" || k === "BrowserBack" || k === "BrowserForward" || (e.altKey && (k === "ArrowLeft" || k === "ArrowRight")) ||
      (mod && ["r", "p", "s", "f", "g", "u", "o", "j", "h"].includes(k)) || k === "F3" || k === "F7") {
    e.preventDefault();
  }
});
main().catch(e => {
  document.getElementById("trace").replaceChildren(el("div", { class: "empty error" }, "The window could not start: " + String(e)));
});
