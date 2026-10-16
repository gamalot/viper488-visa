// An instrument's console, in a window of its own (the main window opens it: `open_console` in src/main.rs, which gives
// this page what to talk to through `console_init`). The log, the history of what was sent and the session are this
// window's; the timeout and the line ending are remembered for every console. Written to its own elements, not rebuilt
// on every entry as the main window's page is: what is typed stays where it is.
"use strict";

const LOG_LIMIT = 500, HISTORY_LIMIT = 200;
const prefs = { timeout: Number(stored("console.timeout", "3000")) || 3000, ending: stored("console.ending", "lf") };
const c = { log: [], history: [], back: -1, busy: false, target: null, used: null, connected: false };
let init = null; // {resource, members: [{resource, online, proto}], via}
let ui = null;   // the elements of the page

// The resource the console goes through: the one chosen, else the one that answered when it was opened.
function target() {
  return c.target || init.via || init.resource;
}

function logPush(entry) {
  c.log.push(entry);
  if (c.log.length > LOG_LIMIT) c.log.splice(0, c.log.length - LOG_LIMIT);
  renderLog();
}

function entryEl(e) {
  return el("div", { class: "entry " + e.dir },
    el("span", { class: "time" }, e.time),
    el("span", { class: "arrow", "aria-hidden": "true" }, { sent: "→", received: "←", info: "·", error: "!" }[e.dir]),
    el("span", { class: "text" }, e.text),
    e.meta ? el("span", { class: "meta" }, e.meta) : null);
}

function renderLog() {
  if (!ui) return;
  ui.log.replaceChildren(...(c.log.length ? c.log.map(entryEl) : [el("div", { class: "hint" }, t("console.empty"))]),
    ...(c.busy ? [el("div", { class: "entry info" }, el("span", { class: "time" }, clock()), el("span", { class: "arrow" }, "…"), el("span", { class: "text" }, t("console.busy")))] : []));
  ui.log.scrollTop = ui.log.scrollHeight;
  ui.clear.disabled = !c.log.length;
}

function setBusy(b) {
  c.busy = b;
  for (const x of ui.buttons) x.disabled = b;
  if (ui.via) ui.via.disabled = b;
  renderLog();
}

// `text`: what to send instead of what is typed (the SYST:ERR? button), which is then left as it is.
async function run(op, text) {
  if (c.busy) return;
  const tgt = target();
  c.used = tgt; // the session this console holds
  const data = text === undefined ? ui.cmd.value : text;
  if ((op === "query" || op === "write") && text === undefined && data.trim()) {
    if (c.history[c.history.length - 1] !== data) c.history.push(data);
    if (c.history.length > HISTORY_LIMIT) c.history.splice(0, c.history.length - HISTORY_LIMIT);
    c.back = -1;
  }
  setBusy(true);
  const time = clock();
  try {
    const x = await invoke("io", { req: { resource: tgt, op, data, timeout_ms: prefs.timeout, ending: prefs.ending } });
    const took = t("console.ms", { ms: x.ms });
    if (x.sent !== null) logPush({ dir: "sent", text: x.sent, time, meta: op === "write" && !x.error ? took : null });
    if (x.received !== null) logPush({ dir: "received", text: x.received, time, meta: t("console.bytes", { n: x.bytes }) + " · " + took });
    if (x.stb !== null) {
      const hex = "0x" + x.stb.toString(16).toUpperCase().padStart(2, "0");
      logPush({ dir: "info", text: t("console.stb", { hex, bin: x.stb.toString(2).padStart(8, "0") }), time, meta: took });
    }
    if (op === "clear" && !x.error) logPush({ dir: "info", text: t("console.cleared"), time, meta: took });
    if (x.error) logPush({ dir: "error", text: x.error, time, meta: took });
    c.connected = x.connected;
  } catch (e) {
    logPush({ dir: "error", text: String(e), time });
  }
  showState();
  setBusy(false);
  ui.cmd.focus();
}

// Another protocol of the instrument: the session to the one before is closed (the window's closing closes the last).
async function through(resource) {
  if (resource === target()) return;
  const before = target();
  c.target = resource;
  c.connected = false;
  showState();
  await invoke("close_session", { resource: before }).catch(() => {});
  if (c.used === before) c.used = null;
  logPush({ dir: "info", text: t("console.through", { name: resource }), time: clock() });
}

// The status bar's left: whether the console holds a session to the instrument (from its first operation until another
// protocol is chosen, the window closes, or the main window closes it because the instrument went).
function showState() {
  statusLeft(el("span", { class: "state" }, el("span", { class: "dot " + (c.connected ? "ok" : "idle") }), t(c.connected ? "console.connected" : "console.disconnected")));
}

// The page, built from the strings of the language in use (again when the language changes: what was typed is kept).
function build() {
  const typed = ui ? ui.cmd.value : "*IDN?";
  const buttons = [];
  const op = (key, name, primary) => {
    const b = el("button", { type: "button", class: primary ? "primary" : null, disabled: c.busy, onclick: () => run(name) }, t(key));
    buttons.push(b);
    return b;
  };
  // Query, write and read are arrows: to the instrument and back, to it, from it; what each does is in its tooltip.
  const arrow = (key, hint, name, paths) => {
    const b = el("button", { type: "button", class: "iconbtn", title: t(hint), "aria-label": t(key), disabled: c.busy, onclick: () => run(name) }, icon(paths, 16));
    buttons.push(b);
    return b;
  };
  const systErr = el("button", { type: "button", class: "mono", title: t("console.syst_err_hint"), disabled: c.busy, onclick: () => run("query", "SYST:ERR?") }, "SYST:ERR?");
  buttons.push(systErr);

  // In the title bar, after "Console": the resource it talks through, a choice where the instrument has several.
  let via = null;
  if (init.members.length > 1) {
    via = el("select", { class: "title-select", "aria-label": t("console.via"), disabled: c.busy, onchange: e => through(e.target.value) });
    for (const x of init.members) via.append(new Option(x.proto + " · " + x.resource, x.resource, false, x.resource === target()));
    setTitleExtra(via);
  } else {
    setTitleExtra(el("span", { class: "title-resource" }, target()));
  }

  const cmd = el("input", { type: "text", class: "mono", placeholder: t("console.placeholder"), "aria-label": t("console.command"), spellcheck: "false", autocomplete: "off" });
  cmd.value = typed;
  cmd.addEventListener("keydown", e => {
    // Enter writes, Ctrl+Enter (Cmd+Enter) queries.
    if (e.key === "Enter") { e.preventDefault(); run(e.ctrlKey || e.metaKey ? "query" : "write"); return; }
    if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
    if (!c.history.length) return;
    e.preventDefault();
    if (e.key === "ArrowUp") c.back = c.back < 0 ? c.history.length - 1 : Math.max(0, c.back - 1);
    else c.back = c.back < 0 || c.back >= c.history.length - 1 ? -1 : c.back + 1;
    cmd.value = c.back < 0 ? "" : c.history[c.back];
  });

  const timeout = el("input", { type: "number", min: "1", max: "600000", step: "500", value: String(prefs.timeout), "aria-label": t("console.timeout"), class: "narrow" });
  timeout.addEventListener("change", () => {
    const v = Math.round(Number(timeout.value));
    if (v >= 1 && v <= 600000) { prefs.timeout = v; store("console.timeout", String(v)); } else timeout.value = String(prefs.timeout);
  });
  const ending = el("select", { "aria-label": t("console.ending"), onchange: e => { prefs.ending = e.target.value; store("console.ending", prefs.ending); } });
  for (const x of ["lf", "crlf", "none"]) ending.append(new Option(t("ending." + x), x, false, x === prefs.ending));

  const clear = el("button", { type: "button", onclick: () => { c.log = []; renderLog(); } }, t("action.clear_log"));
  const log = el("div", { class: "log mono", role: "log", "aria-live": "polite" });

  ui = { cmd, log, clear, via, buttons };
  // `replaceChildren` would write a null as the word "null" (the page's own `el` leaves it out): no null in the list.
  document.getElementById("console").replaceChildren(...[
    el("div", { class: "row" }, cmd,
      arrow("action.query", "console.query_hint", "query", ["M4 12h16", "M8 8l-4 4 4 4", "M16 8l4 4-4 4"]),
      arrow("action.write", "console.write_hint", "write", ["M5 12h14", "M13 6l6 6-6 6"]),
      arrow("action.read", "console.read_hint", "read", ["M19 12H5", "M11 6l-6 6 6 6"])),
    el("div", { class: "row" },
      op("action.stb", "stb"), systErr, op("action.clear_device", "clear"), clear,
      el("span", { class: "spacer" }),
      el("label", { class: "inline" }, el("span", { class: "k" }, t("console.timeout")), timeout),
      el("label", { class: "inline" }, el("span", { class: "k" }, t("console.ending")), ending)),
    log].filter(x => x !== null));
  renderLog();
  showState();
  // The bar itself says "Console"; the system's title (the taskbar, Alt+Tab) names the instrument too.
  setWindowTitle(t("console.window_title", { name: init.resource }));
  const shown = document.querySelector(".titlebar .wintitle");
  if (shown) shown.textContent = t("console.title");
}

// ---- the browser's own keys and menu: none of them has a place here (Reload would drop the log) ----------

document.addEventListener("contextmenu", e => {
  if (e.target.closest && e.target.closest('input[type="text"], input[type="number"], textarea')) return; // cut, copy, paste
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
}, true);

// ---- start ---------------------------------------------------------------------------------------

async function main() {
  await startWindow();
  statusBar();
  init = await invoke("console_init");
  c.target = init.via;
  build();
  ui.cmd.focus();
  ui.cmd.select();

  followChanges(build);
  // The main window closed the session of an instrument that stopped answering.
  window.__TAURI__.event.listen("console-gone", e => {
    const gone = (e.payload && e.payload.resources) || [];
    if (c.used && gone.includes(c.used)) {
      logPush({ dir: "info", text: t("console.closed_gone", { name: c.used }), time: clock() });
      if (c.used === target()) { c.connected = false; showState(); }
    }
  });
}
main().catch(e => {
  document.getElementById("console").replaceChildren(el("div", { class: "empty error" }, "The console could not start: " + String(e)));
});
