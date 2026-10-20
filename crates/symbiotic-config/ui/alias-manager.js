// The aliases window (alias-manager.js, aliases.html): a table of the aliases (aliases.conf) and of the resource names the main
// window knows that have none, one a row: the alias, with whether its instrument is there, the resource name, how that name is
// listed to applications (instruments.conf's expose=), and square buttons: change and remove an alias, or, on a name without
// one, + to give it one (all three through the dialog in common.js). The main window opens it (`open_aliases` in src/main.rs) from Settings, and is told when an alias changed.
"use strict";

let aliasList = [];       // [{alias, resource, group}]: group, the group's alias for a network instrument
let identityList = [];    // [{alias, vendor, model, serial}]
let knownResources = [];  // [{resource, online}]: the instruments' resource names the main window knows
let exposeOf = new Map();  // resource name, lower case -> expose= as set (name, both, none), none for the default
let binding = false;      // features.conf's alias_binding: the units, and the buttons that bind and unbind

async function reload() {
  let set;
  [aliasList, identityList, set] = await Promise.all([invoke("aliases"), invoke("identities"), invoke("instrument_settings")]);
  exposeOf = new Map(set.filter(x => x.expose).map(x => [x.resource.toLowerCase(), x.expose]));
  render();
}
// How applications see a resource name when they list resources: its name, its alias, both or neither. Alias only is
// the default, which is written as no setting.
const EXPOSE = ["name", "alias", "both", "none"];
const PLUS = ["M12 5v14", "M5 12h14"];
const PENCIL = ["M12 20h9", "M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"];
const TRASH = ["M3 6h18", "M8 6V4h8v2", "M19 6l-1 14H6L5 6", "M10 11v6", "M14 11v6"];
// Bind (a closed lock, the one beside a bound alias in the instrument's page) and unbind (the lock open).
const LOCK = ["rect x=5 y=11 width=14 height=10 rx=2", "M8 11V7a4 4 0 0 1 8 0v4"];
const UNLOCK = ["rect x=5 y=11 width=14 height=10 rx=2", "M8 11V7a4 4 0 0 1 7.8-1.3"];
async function setExpose(resource, w) {
  try {
    await invoke("set_expose", { resource, expose: w === "alias" ? null : w });
    toast(t("names.saved"));
  } catch (e) { toast(String(e), true); }
  await changed();
}
async function bind(alias) {
  try {
    const u = await invoke("bind_alias", { alias });
    toast(t("unit.done", { alias, vendor: u.vendor, model: u.model, serial: u.serial }));
  } catch (e) { toast(String(e), true); }
  await changed();
}
async function unbind(alias) {
  try {
    await invoke("unbind_alias", { alias });
    toast(t("unit.unbound", { alias }));
  } catch (e) { toast(String(e), true); }
  await changed();
}
async function changed() {
  await reload();
  await window.__TAURI__.event.emit("aliases-changed", {});
}

// The alias's new name for the same resource name: the new one is made before the old one goes, so a name that is refused
// leaves the alias as it was. (Written with set_alias, not set_names, which knows one alias a resource name.)
function edit(a) {
  aliasDialog(t("alias.edit"), [a.resource], a.alias, t("action.save"), async (resource, name) => {
    if (name === a.alias) return;
    await invoke("set_alias", { alias: name, resource });
    if (name.toLowerCase() !== a.alias.toLowerCase()) await invoke("set_alias", { alias: a.alias, resource: null });
    toast(t("alias.renamed", { from: a.alias, alias: name }));
    await changed();
  });
}
async function remove(alias) {
  try {
    await invoke("set_alias", { alias, resource: null });
    toast(t("alias.removed", { alias }));
  } catch (e) { toast(String(e), true); }
  await changed();
}

function unitOfAlias(alias) {
  return identityList.find(u => u.alias.toLowerCase() === alias.toLowerCase());
}
function unitText(alias) {
  const u = binding && unitOfAlias(alias);
  return u ? el("div", { class: "sub" }, t("unit.bound", { vendor: u.vendor, model: u.model, serial: u.serial })) : null;
}

function isOnline(resource) {
  return knownResources.some(x => x.online && x.resource.toLowerCase() === resource.toLowerCase());
}
// A name with no alias: + gives it one. How it is listed stays as it is.
function add(resource) {
  aliasDialog(t("alias.add"), [resource], "", t("action.add"), async (r, name) => {
    await invoke("set_names", { resource: r, alias: name, expose: exposeOf.get(r.toLowerCase()) || null });
    toast(t("alias.added", { alias: name }));
    await changed();
  });
}
// The rows in order: each known resource name with its aliases, or alone when it has none; then the aliases of names the main
// window does not know (an instrument gone, or never seen).
function entries() {
  const out = [], used = new Set();
  for (const k of knownResources) {
    const mine = aliasList.filter(a => a.resource.toLowerCase() === k.resource.toLowerCase());
    for (const a of mine) used.add(a);
    if (mine.length) out.push(...mine); else out.push({ alias: null, resource: k.resource });
  }
  return out.concat(aliasList.filter(a => !used.has(a)));
}
function render() {
  document.title = t("alias.all");
  statusLeft(t("count.aliases", { n: aliasList.length }));
  const rows = entries().map(a => {
    const on = isOnline(a.resource);
    if (!a.alias) {
      // Without an alias it is listed by its name or not at all (a serial port nobody wants in the list). Its name is the
      // default, written as no setting, so that an alias given later is listed in its place.
      const listed = el("select", { "aria-label": t("names.expose_for", { resource: a.resource }) });
      const now = exposeOf.get(a.resource.toLowerCase()) === "none" ? "none" : "name";
      for (const w of ["name", "none"]) listed.append(new Option(t("expose." + w), w, false, w === now));
      listed.addEventListener("change", () => setExpose(a.resource, listed.value === "none" ? "none" : "alias"));
      return el("tr", { class: "no-alias" },
        el("td", { class: "alias" }, el("span", { class: "dot " + (on ? "ok" : "idle"), title: t(on ? "instrument.online" : "instrument.offline") }),
          el("span", { class: "mono none", title: t("names.no_alias") }, "-")),
        el("td", {}, el("span", { class: "mono" }, a.resource)),
        el("td", {}, listed),
        el("td", { class: "actions" },
          el("button", { type: "button", class: "iconbtn", title: t("alias.add"), "aria-label": t("alias.add_for", { resource: a.resource }), onclick: () => add(a.resource) }, icon(PLUS))));
    }
    const expose = el("select", { "aria-label": t("names.expose_for", { resource: a.resource }) });
    const now = exposeOf.get(a.resource.toLowerCase()) || "alias";
    for (const w of EXPOSE) expose.append(new Option(t("expose." + w), w, false, w === now));
    expose.addEventListener("change", () => setExpose(a.resource, expose.value));
    return el("tr", {},
      el("td", { class: "alias" }, el("span", { class: "dot " + (on ? "ok" : "idle"), title: t(on ? "instrument.online" : "instrument.offline") }),
        el("span", { class: "mono" }, a.alias)),
      el("td", {}, el("span", { class: "mono" }, a.resource), a.group ? el("div", { class: "sub" }, t("alias.group")) : null, unitText(a.alias)),
      el("td", {}, expose),
      el("td", { class: "actions" },
        !binding ? null : unitOfAlias(a.alias)
          ? el("button", { type: "button", class: "iconbtn", title: t("unit.unbind"), "aria-label": t("unit.unbind_named", { alias: a.alias }), onclick: () => unbind(a.alias) }, icon(UNLOCK))
          // Binding asks the instrument *IDN?: not while it is offline (unbinding asks nothing, and stays).
          : el("button", { type: "button", class: "iconbtn", disabled: !on, title: on ? t("unit.bind") + "\n" + t("unit.bind_hint") : t("unit.bind_offline"), "aria-label": t("unit.bind_named", { alias: a.alias }), onclick: () => bind(a.alias) }, icon(LOCK)),
        el("button", { type: "button", class: "iconbtn", title: t("alias.edit"), "aria-label": t("alias.edit_named", { alias: a.alias }), onclick: () => edit(a) }, icon(PENCIL)),
        el("button", { type: "button", class: "iconbtn", title: t("action.remove"), "aria-label": t("alias.remove", { alias: a.alias }), onclick: () => remove(a.alias) }, icon(TRASH))));
  });
  // `replaceChildren` writes a null as the word "null" (the page's own `el` leaves it out): no null in the list.
  document.getElementById("aliases").replaceChildren(...[
    el("div", { class: "console-head" }, el("h1", {}, t("alias.all")), helpMark(t("alias.hint"))),
    rows.length
      ? el("div", { class: "alias-table" }, el("table", {},
          el("thead", {}, el("tr", {}, el("th", {}, t("names.alias")), el("th", {}, t("alias.resource")), el("th", {}, t("names.expose")), el("th", {}))),
          el("tbody", {}, rows)))
      : el("div", { class: "hint" }, t("alias.none"))]);
}

async function main() {
  await startWindow();
  statusBar();
  knownResources = await invoke("aliases_init");
  binding = await invoke("features").then(f => f.alias_binding, () => true);
  await reload();
  setWindowTitle(t("alias.all"));

  followChanges(render);
  // An alias may have been set on an instrument's page in the main window: look again when this window is come back to.
  window.addEventListener("focus", () => { reload().catch(() => {}); });
  // The main window saw an instrument come or go, or opened this one again.
  window.__TAURI__.event.listen("aliases-resources", e => { knownResources = e.payload || []; render(); });
}
main().catch(e => {
  document.getElementById("aliases").replaceChildren(el("div", { class: "empty error" }, "The window could not start: " + String(e)));
});
