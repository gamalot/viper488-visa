// What the configuration window and the console windows share: the call into Rust, the language's strings, the
// theme, the zoom and the page's own element builder. A classic script: what is declared here is global to
// the page's other script (app.js or console.js).
"use strict";
const invoke = window.__TAURI__.core.invoke;

// Each language names itself in its own file ("language.name"): no language's text lives in code.
const LANGUAGES = ["en", "zh-CN"];
let strings = {};
let plural = new Intl.PluralRules("en");

function stored(key, fallback) {
  try { return localStorage.getItem(key) || fallback; } catch (e) { return fallback; }
}
function store(key, value) {
  try { localStorage.setItem(key, value); } catch (e) { /* a per-viewer convenience only */ }
}

// A string with {name} filled in. A string that depends on a count is an object of plural forms
// ({"one": …, "other": …}), chosen by vars.n under the language's own rules.
function t(key, vars) {
  let s = strings[key];
  if (s && typeof s === "object") s = s[plural.select(vars && vars.n)] || s.other;
  if (typeof s !== "string") s = key;
  for (const [k, v] of Object.entries(vars || {})) s = s.split("{" + k + "}").join(v);
  return s;
}

// The same with {name} filled in by nodes (a command in <code>, say), for el's children.
function tNodes(key, nodes) {
  return t(key).split(/\{(\w+)\}/).map((x, i) => i % 2 ? (nodes[x] || "{" + x + "}") : x);
}

function el(tag, attrs, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else if (k in e && typeof v !== "string") e[k] = v;
    else e.setAttribute(k, v);
  }
  for (const c of children.flat()) if (c !== null && c !== undefined && c !== false) e.append(c);
  return e;
}

// Each language is loaded from its own file; `hooks.language` is what the page does after one is in.
async function loadLanguage(lang, keep = true) {
  strings = await (await fetch("locales/" + lang + ".json")).json();
  plural = new Intl.PluralRules(lang);
  document.documentElement.lang = lang;
  for (const e of document.querySelectorAll("[data-i18n]")) e.textContent = t(e.dataset.i18n);
  for (const e of document.querySelectorAll("[data-i18n-title]")) e.title = t(e.dataset.i18nTitle);
  for (const e of document.querySelectorAll("[data-i18n-aria-label]")) e.setAttribute("aria-label", t(e.dataset.i18nAriaLabel));
  if (keep) store("language", lang);
}

// The language the user chose (the main window stores it), else the system's.
function storedLanguage() {
  const guess = (navigator.language || "en").toLowerCase().startsWith("zh") ? "zh-CN" : "en";
  const l = stored("language", guess);
  return LANGUAGES.includes(l) ? l : "en";
}

// A window of its own (a console, the aliases, the pictures, the I/O trace) starts in the language and the theme the
// main window is in: this run's start options (--lang, --theme) if it was given them, else what the user chose; and at
// the zoom. Nothing is stored: those are the main window's. A change made there later comes through `followChanges`.
async function startWindow() {
  let o = {};
  try { o = await invoke("start_options"); } catch (e) { /* none: what is stored */ }
  const lang = o.lang && LANGUAGES.includes(o.lang) ? o.lang : storedLanguage();
  setTheme(o.theme || stored("theme", "system"), false);
  if (zoom !== 100) setZoom(zoom, false);
  await loadLanguage(lang, false);
}

// The main window changed the theme or the language (it stores them): this window follows; `relabel` redraws its text.
function followChanges(relabel) {
  window.addEventListener("storage", async e => {
    if (e.key === "theme") setTheme(stored("theme", "system"), false);
    else if (e.key === "language") { await loadLanguage(storedLanguage(), false); relabel(); }
  });
}

// `keep` false: for this run only (a start option), not remembered as the user's choice.
function setTheme(theme, keep = true) {
  if (theme === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
  if (keep) store("theme", theme);
}


// A modal question; true when the user chose `ok`.
function confirmDialog(title, text, ok, danger) {
  return new Promise(done => {
    const d = el("dialog", { class: "dialog" },
      el("h2", {}, title),
      el("div", { class: "dialog-body" }, text),
      el("div", { class: "row end" },
        el("button", { type: "button", onclick: () => d.close("") }, t("action.cancel")),
        el("button", { type: "button", class: danger ? "danger-button" : "primary", onclick: () => d.close("ok") }, ok)));
    d.addEventListener("close", () => { done(d.returnValue === "ok"); d.remove(); });
    document.body.append(d);
    d.showModal();
  });
}

function box(title, action, ...body) {
  return el("section", { class: "box" },
    el("div", { class: "bar" }, el("h2", {}, title), action || null),
    el("div", { class: "pad" }, ...body));
}

// What each page says about things that happen here (a line for the user, a zoom that changed): the page sets them.
const hooks = { say: (text, error) => toast(text, error), zoomed: () => {} };

function clock() {
  const d = new Date();
  return [d.getHours(), d.getMinutes(), d.getSeconds()].map(x => String(x).padStart(2, "0")).join(":");
}

// ---- zoom ------------------------------------------------------------------------------------------

// The page's zoom, as a browser's: Ctrl+plus/minus step through these, Ctrl+0 goes back to 100 %, and
// Ctrl+wheel does the same; Settings has it too. The webview's own zoom (Tauri's setZoom), so the page
// lays itself out again and every coordinate stays right; remembered for the next start (and shared with the
// console windows: the same page origin shares the setting).
const ZOOMS = [50, 67, 75, 80, 90, 100, 110, 125, 150, 175, 200, 250, 300];
let zoom = Number(stored("zoom", "100")) || 100;

async function setZoom(percent, say) {
  zoom = Math.min(ZOOMS[ZOOMS.length - 1], Math.max(ZOOMS[0], percent));
  store("zoom", String(zoom));
  try { await window.__TAURI__.webview.getCurrentWebview().setZoom(zoom / 100); }
  catch (e) { hooks.say(String(e), true); return; }
  if (say) hooks.say(t("toast.zoom", { percent: zoom }));
  hooks.zoomed();
}
function zoomStep(dir) {
  const i = ZOOMS.findIndex(z => z >= zoom);
  const at = i < 0 ? ZOOMS.length - 1 : i;
  // Between two steps (a value set by hand): the next step in that direction.
  const next = dir > 0 ? (ZOOMS[at] > zoom ? ZOOMS[at] : ZOOMS[Math.min(at + 1, ZOOMS.length - 1)]) : ZOOMS[Math.max(at - 1, 0)];
  setZoom(next, true);
}
document.addEventListener("wheel", e => {
  if (!(e.ctrlKey || e.metaKey)) return;
  e.preventDefault();
  zoomStep(e.deltaY < 0 ? 1 : -1);
}, { passive: false });


// ---- the window's own title bar ----------------------------------------------------------------------

// The system's title bar is gone (Windows, Linux) or lies over the page (macOS, whose three buttons stay). The main
// window's toolbar is its title bar; a window of its own gets a slim one with its title. Dragging, and a double click
// to maximize, are Tauri's: data-tauri-drag-region="deep" is anywhere in the bar but on a control.
const PLATFORM = /mac/i.test(navigator.platform) ? "mac" : /win/i.test(navigator.platform) ? "windows" : "linux";
document.documentElement.dataset.platform = PLATFORM;

function glyph(size, ...shapes) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  for (const [k, v] of Object.entries({ width: size, height: size, viewBox: `0 0 ${size} ${size}`, fill: "none", stroke: "currentColor", "stroke-width": 1, "aria-hidden": "true" })) svg.setAttribute(k, v);
  for (const [tag, attrs] of shapes) {
    const e = document.createElementNS("http://www.w3.org/2000/svg", tag);
    for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
    svg.append(e);
  }
  return svg;
}

// Minimize, maximize (restore when maximized) and close, drawn as Windows draws them; none on macOS.
function windowControls() {
  if (PLATFORM === "mac") return null;
  const w = window.__TAURI__.window.getCurrentWindow();
  const button = (cls, key, onclick, ...shapes) =>
    el("button", { type: "button", class: cls, tabindex: "-1", "data-i18n-title": key, "data-i18n-aria-label": key, onclick }, glyph(10, ...shapes));
  const max = button("max", "window.maximize", () => w.toggleMaximize(), ["rect", { x: .5, y: .5, width: 9, height: 9, rx: 1 }]);
  const restore = button("restore", "window.restore", () => w.toggleMaximize(),
    ["rect", { x: .5, y: 2.5, width: 7, height: 7, rx: 1 }], ["path", { d: "M2.5 2.5V1.5a1 1 0 0 1 1-1h5a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1h-1" }]);
  const maximized = async () => {
    try { document.documentElement.dataset.maximized = await w.isMaximized(); } catch (e) { /* the maximize glyph stays */ }
  };
  maximized();
  w.onResized(maximized);
  return el("div", { class: "winctl" },
    button("min", "window.minimize", () => w.minimize(), ["path", { d: "M0 5.5h10" }]),
    max, restore,
    button("close", "window.close", () => w.close(), ["path", { d: "M.5.5l9 9M9.5.5l-9 9" }]));
}

// A window that is not in front shows it in its bar, as the system's would.
window.addEventListener("blur", () => { document.documentElement.dataset.inactive = "true"; });
window.addEventListener("focus", () => { delete document.documentElement.dataset.inactive; });

(function ownTitleBar() {
  const toolbar = document.querySelector(".toolbar");
  if (toolbar) {
    toolbar.dataset.tauriDragRegion = "deep";
    const c = windowControls();
    if (c) toolbar.append(c);
    return;
  }
  document.body.prepend(el("header", { class: "titlebar", "data-tauri-drag-region": "deep" },
    el("span", { class: "logo", "aria-hidden": "true" }, glyph(24, ["path", { d: "M3 12h3.5l2.2-6.5 4.6 13 2.2-6.5H21", "stroke-width": 2.6, "stroke-linecap": "round", "stroke-linejoin": "round" }])),
    el("span", { class: "wintitle" }, document.title),
    el("span", { class: "title-extra" }),
    el("span", { class: "spacer" }),
    windowControls()));
})();

// A window's title, in its own bar and the system's (the taskbar, Alt+Tab). The window was made with it (the open_*
// commands); on macOS the system's is left as it was made, because AppKit puts its three buttons back in their own place
// whenever the title changes, after tao has moved them down into the middle of the bar.
function setWindowTitle(text) {
  document.title = text;
  const shown = document.querySelector(".titlebar .wintitle");
  if (shown) shown.textContent = text;
  if (PLATFORM === "mac") return;
  try { window.__TAURI__.window.getCurrentWindow().setTitle(text); } catch (e) { /* the page's title will do */ }
}
// What a window puts in its own title bar after the title (the console's choice of protocol); macOS's bar is the page's too.
// Pointing at it does not drag the window.
function setTitleExtra(...nodes) {
  const slot = document.querySelector(".titlebar .title-extra");
  if (!slot) return;
  for (const n of nodes) if (n && n.setAttribute) n.setAttribute("data-tauri-drag-region", "false");
  slot.replaceChildren(...nodes.filter(Boolean));
}

// An SVG icon from path data; "rect k=v …" and "circle k=v …" make those shapes.
function icon(paths, size = 14) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  for (const [k, v] of Object.entries({ width: size, height: size, viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": 2, "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true" })) svg.setAttribute(k, v);
  for (const d of paths) {
    const shape = /^(rect|circle) /.exec(d);
    const e = document.createElementNS("http://www.w3.org/2000/svg", shape ? shape[1] : "path");
    if (shape) for (const [k, v] of d.slice(shape[0].length).split(" ").map(x => x.split("="))) e.setAttribute(k, v);
    else e.setAttribute("d", d);
    svg.append(e);
  }
  return svg;
}
// A small button that copies `text` (a resource name) beside where it is shown.
function copyButton(text) {
  return el("button", { type: "button", class: "copy", title: t("action.copy"), "aria-label": t("action.copy_named", { name: text }),
    onclick: e => { e.stopPropagation(); copyText(text); } },
    icon(["rect x=9 y=9 width=12 height=12 rx=2", "M5 15V5a2 2 0 0 1 2-2h10"]));
}
// Where something is kept (the configuration folder, the service's log, the pictures): its name, the path, a copy button.
// Every window shows a path this one way.
function pathLine(label, path) {
  return el("div", { class: "pathline" }, el("span", { class: "muted" }, label), el("span", { class: "mono" }, path), copyButton(path));
}
// Copied, and said so in the window's own toast.
async function copyText(text) {
  try { await navigator.clipboard.writeText(text); }
  catch (e) {
    const area = el("textarea", { class: "offscreen" });
    area.value = text;
    document.body.append(area);
    area.select();
    document.execCommand("copy");
    area.remove();
  }
  toast(t("toast.copied", { text: text.length > 60 ? text.slice(0, 57) + "…" : text }));
}

// What an action did: a line in the window's status bar (statusBar) for a few seconds, an error longer; in a window without
// one, a note that floats over its foot.
let toastTimer = null;
function toast(text, error) {
  const bar = document.getElementById("toast");
  if (bar) {
    bar.textContent = text;
    bar.title = text; // whole, where the bar cuts it short
    bar.classList.toggle("error", !!error);
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { bar.textContent = ""; bar.title = ""; }, error ? 8000 : 5000);
    return;
  }
  let e = document.getElementById("float-toast");
  if (!e) document.body.append(e = el("div", { id: "float-toast", role: "status" }));
  e.textContent = text;
  e.classList.toggle("error", !!error);
  e.classList.add("shown");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => e.classList.remove("shown"), error ? 8000 : 4000);
}

// A window's status bar, the main window's kind (index.html has its own in the page): on the left what the window is about
// (statusLeft), then a short line and the messages (toast), and the version on the right unless features.conf hides it.
function statusBar() {
  if (document.getElementById("toast")) return;
  document.body.append(el("footer", { class: "statusbar" },
    el("span", { class: "status-left", id: "status-left" }),
    el("span", { id: "toast", role: "status" }),
    el("span", { class: "spacer" }),
    el("span", { id: "version" })));
  invoke("features").then(f => f.version ? invoke("about") : null).then(a => {
    statusVersion = a ? a.version : null;
    showVersion();
  }, () => {});
}
let statusVersion = null; // the version the status bar shows, when features.conf does not hide it
function showVersion() {
  const v = document.getElementById("version");
  if (v) v.textContent = statusVersion ? t("splash.version", { v: statusVersion }) : "";
}
// The status bar's left part; the version is written again with it, in the language now in use.
function statusLeft(...parts) {
  const e = document.getElementById("status-left");
  if (e) e.replaceChildren(...parts.filter(Boolean));
  showVersion();
}

// The long explanation of a panel or a switch: a "?" beside it that shows it while pointed at or focused, instead of a
// paragraph under it. Its explanation opens toward the side with more room in the pane around it, so it is not cut off
// by the tree.
function helpMark(text) {
  const h = el("span", { class: "help", tabindex: "0", role: "img", "aria-label": text, "data-tip": text }, "?");
  const side = () => {
    const pane = (h.closest(".detail, .dialog, main") || document.body).getBoundingClientRect();
    const r = h.getBoundingClientRect();
    h.dataset.open = r.right - pane.left >= pane.right - r.left ? "left" : "right";
  };
  h.addEventListener("mouseenter", side);
  h.addEventListener("focus", side);
  return h;
}

// The dialog that adds an alias (the instrument page's +) or changes one (the aliases window): a resource name, from `resources`
// (one, fixed, when an alias is changed), and the alias, `alias` to begin with. `save(resource, alias)` does it; what it throws
// is said in the dialog, which stays open, and the dialog closes once it is done.
function aliasDialog(title, resources, alias, okText, save) {
  const resource = el("select", { id: "alias-dialog-resource", disabled: resources.length < 2 }, resources.map(r => new Option(r, r)));
  const input = el("input", { type: "text", id: "alias-dialog-name", class: "mono", spellcheck: "false", autocomplete: "off", placeholder: t("names.alias_placeholder") });
  input.value = alias;
  const message = el("div", { class: "error", role: "status" });
  const ok = el("button", { type: "button", class: "primary", onclick: done }, okText);
  input.addEventListener("keydown", e => { if (e.key === "Enter") done(); });
  const d = el("dialog", { class: "dialog alias-dialog" },
    el("h2", {}, title),
    el("div", { class: "form" },
      el("label", { for: "alias-dialog-resource", class: "k" }, t("alias.resource")), resource,
      el("label", { for: "alias-dialog-name", class: "k" }, t("names.alias")), input),
    message,
    el("div", { class: "row end" }, el("button", { type: "button", onclick: () => d.close() }, t("action.cancel")), ok));
  d.addEventListener("close", () => d.remove());
  document.body.append(d);
  d.showModal();
  input.focus();
  input.select();
  async function done() {
    const name = input.value.trim();
    if (!name) { message.textContent = t("alias.empty"); input.focus(); return; }
    ok.disabled = true;
    try { await save(resource.value, name); } catch (e) { message.textContent = String(e); ok.disabled = false; return; }
    d.close();
  }
}
