// The page: text from the locale files (locales/<lang>.json), colors from style.css's variables, data
// from the Rust commands in src/main.rs through Tauri's in-process channel.
"use strict";
// What is shared with the console windows (the call into Rust, strings, theme, zoom) is in common.js.

async function setLanguage(lang, keep = true) {
  await loadLanguage(lang, keep);
  document.title = t("app.title");
  render();
}

// ---- state -----------------------------------------------------------------------------------

let snap = null;          // the last snapshot: {boards, resources, lan_configured}, or {error}
let settings = new Map(); // resource -> {idn, name}: only those that differ from the defaults
let idn = new Map();      // resource -> {pending} | {text} | {error}: this run's *IDN? answers
let selected = { kind: "start" }; // what the right side shows: the start page until the user picks something in the tree
let about = null;
// Which features this window shows (features.conf: sharing, the service page, the I/O trace, an instrument's display name,
// binding an alias to a unit, the version in the status bar, the counts in the tree); each at its default until told.
let features = { sharing: false, service: true, trace: false, display_name: false, alias_binding: false, version: true, tree_counts: false };
let listAll = false; // viFindRsrc lists the absent instruments too (instruments.conf, list=all)

let aliasList = [];      // [{alias, resource, group}]: aliases.conf, and the group's for network instruments (group: true)
let identityList = [];   // [{alias, vendor, model, serial}]: the aliases bound to one unit (identities.conf, and the group's)
// Whether each network instrument known only by its group alias (found neither by the search nor in lan.conf) can be reached from
// here, by lower-cased resource name: this computer looks for itself (lan_reach), as for a configured one.
const groupOnline = new Map();

// What a pane loads when it is first shown (the LAN check, the serial ports, ...), by key; cleared by
// Refresh. {loading} | {value} | {error}.
const extra = new Map();
function need(key, cmd, args) {
  if (!extra.has(key)) {
    extra.set(key, { loading: true });
    invoke(cmd, args || {}).then(v => extra.set(key, { value: v }), e => extra.set(key, { error: String(e) })).then(render);
  }
  return extra.get(key);
}

// An action under way, by key (its button is disabled meanwhile), and what the last one said: beside its control, or with
// `inStatus` in the status bar (the names panel's, whose line would push its fields about).
const busy = new Set();
const notes = new Map(); // key -> {text, error}
async function act(key, fn, done, inStatus) {
  if (busy.has(key)) return;
  busy.add(key);
  render();
  let note;
  try { const v = await fn(); note = done ? { text: done(v), error: false } : null; }
  catch (e) { note = { text: String(e), error: true }; }
  busy.delete(key);
  if (note && inStatus) toast(note.text, note.error);
  else if (note) notes.set(key, note); else notes.delete(key);
  render();
}
function noteEl(key) {
  const n = notes.get(key);
  return n ? el("span", { class: n.error ? "error" : "muted", role: "status" }, n.text) : null;
}

function settingsMap(list) {
  return new Map(list.map(x => [x.resource, { idn: x.idn, name: x.name, hidden: x.hidden, expose: x.expose || undefined }]));
}
// A LAN host whose MAC is not the one its settings are for (units_check): its settings do not count here either, and
// its notice says what it keeps.
let unconfirmedHosts = new Set();
function settingsOf(r) {
  const none = { idn: true, name: null, hidden: false };
  if (interfaceOf(r).group === "lan" && unconfirmedHosts.has(lanHost(r))) return none;
  return settings.get(r) || none;
}

// Hidden instruments (instruments.conf): left out of the tree and never asked *IDN?, unless the
// user shows them (a per-viewer convenience, remembered). other programs' lists are the names panel's business.
let showHidden = stored("tree.showHidden", "") === "1";

// May the window ask this instrument *IDN? on its own (when opened or refreshed)?
function mayAsk(it) {
  const s = settingsOf(it.resource);
  return it.online && s.idn && !s.hidden && interfaceOf(it.resource).group !== "serial";
}

// "GPIB0::6::INSTR" and friends: what kind of thing, and which group of the tree it goes in.
function interfaceOf(r) {
  let m = /^GPIB(\d+)::/.exec(r);
  if (m) return { group: "gpib", board: "GPIB" + m[1] };
  if (r.startsWith("USB")) return { group: "usb" };
  if (r.startsWith("TCPIP")) return { group: "lan" };
  if (r.startsWith("ASRL")) return { group: "serial" };
  return { group: "other" };
}
// A resource name's parts between "::", an IPv6 host in brackets kept whole ("TCPIP::[fe80::1]::inst0::INSTR").
function resParts(r) {
  const out = [];
  let cur = "", depth = 0;
  for (let i = 0; i < r.length; i++) {
    if (r[i] === "[") depth++;
    else if (r[i] === "]") depth = Math.max(0, depth - 1);
    else if (depth === 0 && r[i] === ":" && r[i + 1] === ":") { out.push(cur); cur = ""; i++; continue; }
    cur += r[i];
  }
  out.push(cur);
  return out;
}
// A LAN resource's protocol, and which of the host's instruments it reaches: inst<k> (VXI-11) and
// hislip<k> are instrument k, a socket instrument 0. A device of another name (a LAN/GPIB gateway's
// gpib0,6) is an instrument of its own: index null.
function lanProto(r) {
  const dev = resParts(r)[2] || "";
  if (/::SOCKET$/i.test(r)) return { proto: "socket", index: 0 };
  let m = /^hislip(\d+)$/i.exec(dev);
  if (m) return { proto: "hislip", index: Number(m[1]) };
  m = /^inst(\d+)$/i.exec(dev);
  if (m) return { proto: "vxi11", index: Number(m[1]) };
  return { proto: "vxi11", index: null };
}
// Whether each gateway answered the last check (lanTick), by host.
const gwOnline = new Map();
// Which addresses of each gateway (interface gpib0) had a listener at the last probe (gatewayTick), by host: a set of numbers.
// A gateway instrument is there when its address has one; where none is known yet, as the gateway is.
const gwListeners = new Map();
function gpibPad(r) {
  const m = /^gpib0,(\d+)/i.exec(resParts(r)[2] || "");
  return m ? Number(m[1]) : null;
}
function isGatewayDevice(r) { return /^gpib\d+,\d+/i.test(resParts(r)[2] || ""); }
function lanHost(r) {
  return (resParts(r)[1] || "").replace(/,\d+$/, "").toLowerCase();
}
// What the tree shows beside a LAN instrument: its host, and the device when that is what tells it
// apart (a gateway's gpib0,6, a second instrument's inst1).
function lanShort(r) {
  const { index } = lanProto(r);
  return index === 0 ? lanHost(r) : lanHost(r) + " · " + (resParts(r)[2] || "");
}
const PROTO_ORDER = { hislip: 0, vxi11: 1, socket: 2 };

// One instrument that answers on several protocols (inst0, hislip0, a socket) is one item: its
// resources (members), the one that names it (the first by protocol: HiSLIP, VXI-11, socket; its
// settings, selection and *IDN? answer go by it), and the one to ask (the first that answers).
function mergeLan(list) {
  const groups = new Map();
  for (const x of list) {
    const { index } = lanProto(x.resource);
    const key = index === null ? x.resource : lanHost(x.resource) + "#" + index;
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(x);
  }
  return [...groups.values()].map(members => {
    const order = x => PROTO_ORDER[lanProto(x.resource).proto] * 1000 + (Number(resParts(x.resource)[2]) || 0);
    members.sort((a, b) => order(a) - order(b));
    const primary = members[0].resource;
    // Settings made on another of its resources (before they were one) count for the instrument.
    if (!settings.has(primary)) {
      const other = members.find(x => settings.has(x.resource));
      if (other) settings.set(primary, { ...settings.get(other.resource), expose: undefined });
    }
    const answering = members.find(x => x.online);
    return {
      resource: primary,
      members,
      via: answering ? answering.resource : primary,
      online: !!answering,
      hidden: settingsOf(primary).hidden,
      configured: members.some(x => x.configured),
    };
  });
}
function isInstrument(r) { return /::(INSTR|SOCKET)$/.test(r); }

// The parts of an *IDN? answer: manufacturer, model, serial, firmware (IEEE 488.2 10.14).
function idnParts(text) {
  const p = text.split(",").map(s => s.trim());
  return { vendor: p[0] || "", model: p[1] || "", serial: p[2] || "", firmware: p.slice(3).join(",") };
}

// What a tree row and a heading call an instrument: the user's name, else its model, else nothing.
function titleOf(r) {
  const s = settingsOf(r);
  if (s.name) return s.name;
  const a = idn.get(r);
  if (a && a.text) { const p = idnParts(a.text); return [p.vendor, p.model].filter(Boolean).join(" ") || a.text; }
  return null;
}

// The tree's contents from a snapshot: GPIB boards with their instruments, then USB, LAN (the
// configured ones that did not answer too) and serial ports.
function model() {
  const found = snap && snap.resources && snap.resources.Ok ? snap.resources.Ok : [];
  const boards = snap && snap.boards && snap.boards.Ok ? snap.boards.Ok : [];
  const out = { boards: [], usb: [], lan: [], gateways: [], serial: [], other: [] };
  for (const b of boards) out.boards.push({ ...b, instruments: [] });
  for (const r of found) {
    if (!isInstrument(r)) continue;
    const i = interfaceOf(r);
    const item = { resource: r, online: true, hidden: settingsOf(r).hidden, configured: false };
    if (i.group === "gpib") {
      let b = out.boards.find(x => x.board === i.board);
      if (!b) { b = { board: i.board, kind: null, serial: "", present: true, instruments: [] }; out.boards.push(b); }
      b.instruments.push(item);
    } else out[i.group].push(item);
  }
  for (const r of (snap && snap.lan_configured) || []) {
    const known = out.lan.find(x => x.resource === r);
    if (known) known.configured = true;
    else out.lan.push({ resource: r, online: false, hidden: settingsOf(r).hidden, configured: true });
  }
  // The group's network instruments this computer did not find: one item each, there when this computer can reach it.
  // Not before there is a snapshot: nothing else is there either.
  for (const r of snap && !snap.error ? groupOnly(out.lan) : []) {
    out.lan.push({ resource: r, online: groupOnline.get(r.toLowerCase()) === true, hidden: settingsOf(r).hidden, configured: false });
  }
  out.lan = mergeLan(out.lan);
  // A LAN/GPIB gateway is a node like a GPIB board: the ones discovery found, and any host with a
  // configured gpib0,N. Its instruments are still ordinary LAN items (settings, *IDN?, the console).
  const gw = new Map();
  const gwOf = ip => { if (!gw.has(ip)) gw.set(ip, { ip, address: null, instruments: [] }); return gw.get(ip); };
  for (const [ip, address] of (snap && snap.lan_gateways) || []) gwOf(ip).address = address;
  for (const it of out.lan) if (isGatewayDevice(it.resource)) gwOf(lanHost(it.resource)).instruments.push(it);
  for (const g of gw.values()) {
    // Seen answering unless a check said otherwise; a configured-only gateway by its instruments.
    const listeners = gwListeners.get(g.ip);
    g.online = gwOnline.has(g.ip) ? gwOnline.get(g.ip) : g.address !== null || listeners !== undefined || g.instruments.some(i => i.online);
    // Its instruments are there when their address has a listener (the gateway being up says nothing of them).
    for (const it of g.instruments) {
      const pad = gpibPad(it.resource);
      let on = it.online;
      if (!g.online) on = false;
      else if (listeners && pad !== null) on = listeners.has(pad);
      it.online = on;
      for (const x of it.members || []) x.online = on;
    }
  }
  out.gateways = [...gw.values()];
  return out;
}

// The resources of the group's network instrument aliases that `lan` (the search's and lan.conf's) does not have.
function groupOnly(lan) {
  const have = new Set(lan.map(x => x.resource.toLowerCase()));
  return [...new Set(aliasList.filter(a => a.group).map(a => a.resource))].filter(r => !have.has(r.toLowerCase()));
}
function groupOnlyNow() {
  const found = snap && snap.resources && snap.resources.Ok ? snap.resources.Ok : [];
  return groupOnly([...found, ...((snap && snap.lan_configured) || [])].map(resource => ({ resource })));
}
// Whether each of them can be reached from here (a connection to it; behind a gateway the gateway's probe says more); true if any changed.
async function reachGroupOnly() {
  const rs = groupOnlyNow();
  if (!rs.length) return false;
  let answers;
  try { answers = await invoke("lan_reach", { resources: rs }); } catch (e) { return false; }
  let changed = false;
  for (const [r, ok] of answers) {
    if (groupOnline.get(r.toLowerCase()) !== ok) changed = true;
    groupOnline.set(r.toLowerCase(), ok);
  }
  return changed;
}
// Every alias again (another computer of the group may have changed one); true if the group's network instruments changed.
async function reloadAliases() {
  try {
    const before = JSON.stringify(aliasList.filter(a => a.group));
    [aliasList, identityList] = await Promise.all([invoke("aliases"), invoke("identities")]);
    return JSON.stringify(aliasList.filter(a => a.group)) !== before;
  } catch (e) { return false; }
}

// The item a resource belongs to: itself, or the LAN instrument it is one protocol of.
function itemOf(m, r) {
  return allInstruments(m).find(x => x.resource === r || (x.members && x.members.some(y => y.resource === r)));
}

function allInstruments(m) {
  return [...m.boards.flatMap(b => b.instruments), ...m.usb, ...m.lan, ...m.serial, ...m.other];
}

// ---- asking the instruments --------------------------------------------------------------------

// One queue per bus, so a board is asked one instrument at a time (they share it); LAN instruments
// each have their own. Serial ports are asked only on request: a port is not always an instrument.
function busOf(r) {
  const i = interfaceOf(r);
  return i.group === "gpib" ? i.board : i.group === "lan" ? r : i.group;
}
async function identifyAll() {
  const queues = new Map();
  for (const it of allInstruments(model())) {
    if (!mayAsk(it)) continue;
    const q = busOf(it.resource);
    if (!queues.has(q)) queues.set(q, []);
    queues.get(q).push(it.resource);
    idn.set(it.resource, { pending: true });
  }
  render();
  await Promise.all([...queues.values()].map(async list => {
    for (const r of list) await identify(r, true);
  }));
}
// `quiet`: asked by the page itself, so an instrument another program is using is not asked now
// (main.rs, `identify`) but once more, IN_USE_RETRY_MS later.
const IN_USE_RETRY_MS = 30000;
async function identify(r, quiet = false) {
  const it = itemOf(model(), r);
  const via = it && it.via ? it.via : r;
  idn.set(r, { pending: true });
  render();
  try {
    const got = await invoke("identify", { resource: via, quiet });
    if (got.in_use) {
      idn.set(r, { inUse: true });
      setTimeout(() => { const a = idn.get(r); if (a && a.inUse) identify(r, true); }, IN_USE_RETRY_MS);
    } else {
      idn.set(r, { text: got.text });
      checkOwner(r, via, got.text);
      followBound();
    }
  } catch (e) { idn.set(r, { error: String(e) }); }
  render();
}

async function refresh() {
  if (serviceStopped) return; // nothing to ask until the service is started again (its own button)
  // One reload at a time: a live update under way finishes first, and none starts meanwhile.
  while (updating) await new Promise(done => setTimeout(done, 100));
  updating = true;
  // Whatever goes wrong, the reload ends: a page that throws while it is drawn must not leave the tree waiting for
  // ever, with every live update held off by `updating`.
  try {
    watchPrint = null;
    snap = null;
    idn = new Map();
    drawn();
    await reloadAll();
    await closeGone();
  } finally {
    updating = false;
  }
  drawn();
  identifyAll();
}
// The user stopped the service on its page: this window starts it again only from that page's Start button
// (start_service), or its own polling would start it at once (client.rs, hold_start). Refresh is off meanwhile.
let serviceStopped = false;
async function stopService() {
  await invoke("stop_service");
  // It gives the hardware back before it lets go of its channel: asked too soon, it still says it runs.
  for (let i = 0; i < 50 && await invoke("service_running").catch(() => false); i++) await new Promise(done => setTimeout(done, 100));
  serviceStopped = true;
  extra.delete("service");
  watchPrint = null;
  snap = { error: t("service.stopped_tree"), stopped: true };
}
async function startService() {
  await invoke("start_service");
  serviceStopped = false;
  extra.delete("service");
  await refresh();
}

// render(), with what it throws reported in the status bar instead of stopping the caller.
function drawn() {
  try { render(); } catch (e) { toast(String(e), true); }
}
async function reloadAll() {
  try {
    extra.clear();
    const [s, list, al, ids, sv] = await Promise.all([invoke("snapshot"), invoke("instrument_settings"), invoke("aliases"), invoke("identities"), invoke("shared_settings")]);
    snap = s;
    sharedViews = new Map(sv.map(x => [x.alias.toLowerCase(), { idn: x.idn, hidden: x.hidden }]));
    aliasList = al;
    identityList = ids;
    settings = settingsMap(list);
    await reachGroupOnly();
    await checkUnits();
    // Who is there behind each gateway, before anything is asked of it.
    await scanGateways();
  } catch (e) { snap = { error: String(e) }; }
}

// After a change that adds or removes resources (a LAN instrument, a controller): the tree again, asking *IDN? only of
// what it did not know. On the first start with the expose model, what was hidden becomes "not exposed"
// (instruments.conf; nothing happens once it has been done).
async function migrateHidden() {
  try {
    const known = allInstruments(model()).flatMap(i => i.members ? i.members.map(x => x.resource) : [i.resource]);
    await invoke("migrate_hidden", { resources: known });
    settings = settingsMap(await invoke("instrument_settings"));
    render();
  } catch (e) { /* tried again at the next start */ }
}

async function reloadSnapshot() {
  try { snap = await invoke("snapshot"); } catch (e) { snap = { error: String(e) }; }
  // What went is asked again when it is back (it may be another instrument at that address).
  const now = new Set(allInstruments(model()).filter(x => x.online).map(x => x.resource));
  // (Not a shared instrument's: it is asked by its alias, which is no resource here.)
  const shared = new Set(sharedByOthers().map(x => x.alias));
  for (const r of [...idn.keys()]) if (!now.has(r) && !shared.has(r) && !idn.get(r).pending) idn.delete(r);
  await checkUnits();
  render();
  for (const it of allInstruments(model())) {
    if (mayAsk(it) && !idn.has(it.resource)) identify(it.resource, true);
  }
  followBound();
}

async function saveSettings(r, next) {
  try {
    // The exposure is the names panel's own: saving the rest keeps it.
    next = { idn: next.idn, name: next.name || null, hidden: !!next.hidden, expose: settingsOf(r).expose };
    await invoke("set_instrument_settings", { settings: { resource: r, ...next } });
    settings.set(r, next);
    const it = allInstruments(model()).find(x => x.resource === r);
    if (it && mayAsk(it) && !idn.has(r)) identify(r, true);
    return null;
  } catch (e) { return String(e); }
}

// ---- bound LAN aliases following their units -------------------------------------------------------

// A bound alias whose LAN address is not its unit any more, while the tree has that unit answering at one other LAN
// address, is moved there, as the DLL would at its next viOpen; the status bar says so. Only the answers the window
// has: nothing is asked for this. Rust checks again before it writes.
const followSaid = new Set(); // what was said of an alias that could not follow: once a run
let following = false;
async function followBound() {
  if (following || !snap || snap.error) return;
  const bound = aliasList.some(a => interfaceOf(a.resource).group === "lan" && unitOf(a.alias));
  if (!bound) return;
  following = true;
  try {
    const places = allInstruments(model()).filter(it => interfaceOf(it.resource).group === "lan").map(it => {
      const a = idn.get(it.resource);
      return { resources: (it.members || [it]).map(x => x.resource), online: !!it.online, idn: a && a.text ? a.text : null };
    });
    let done;
    try { done = await invoke("follow_bound", { places }); } catch (e) { return; }
    let moved = false;
    for (const f of done) {
      if (f.moved) {
        moved = true;
        // With the old address's settings, if it had any, and what they replaced at the new one; a failure to move them is said too.
        const said = [t("follow.moved", { alias: f.alias, to: f.to[0] }),
          f.settings.length ? t("follow.settings", { n: f.settings.length }) : null,
          f.replaced.length ? t("follow.replaced", { list: f.replaced.join(", ") }) : null,
          f.error ? f.error : null].filter(Boolean).join(document.documentElement.lang.startsWith("zh") ? "" : " ");
        toast(said, !!f.error);
        continue;
      }
      const key = f.alias + "|" + f.to.join(",");
      if (followSaid.has(key)) continue;
      followSaid.add(key);
      toast(f.error ? t("follow.failed", { alias: f.alias, to: f.to[0], error: f.error }) : t("follow.ambiguous", { alias: f.alias, list: f.to.join(", ") }), !!f.error);
    }
    if (moved) {
      await reloadAliases();
      await window.__TAURI__.event.emit("aliases-changed", {});
      render();
    }
  } finally {
    following = false;
  }
}

// ---- LAN settings follow the MAC ----------------------------------------------------------------------

// The tree's answering LAN resources, by their host's MAC (units_check, which asks no instrument anything): whose settings count, what
// a host with another instrument keeps (its notice, as for an *IDN? answer of another unit), and what went with its MAC to another host.
let checkingUnits = false;
async function checkUnits() {
  if (checkingUnits || !snap || snap.error) return;
  checkingUnits = true;
  try {
    const items = allInstruments(model()).filter(it => interfaceOf(it.resource).group === "lan");
    const online = items.flatMap(it => (it.members || [it]).filter(x => x.online).map(x => x.resource));
    let st;
    try { st = await invoke("units_check", { resources: online }); } catch (e) { return; }
    unconfirmedHosts = new Set(st.unconfirmed);
    for (const c of st.changed) {
      for (const it of items.filter(x => lanHost(x.resource) === c.host)) {
        const a = idn.get(it.resource);
        ownerChanged.set(it.resource, { was: c.was, wasMac: c.was_mac, nowMac: c.now_mac, settings: c.settings, aliases: c.aliases, idn: a && a.text ? a.text : "", via: it.via || it.resource });
        if (!ownerSaid.has(it.resource)) {
          ownerSaid.add(it.resource);
          toast(t("owner.toast", { host: c.host, was: c.was && c.was.model ? unitName(c.was) : t("owner.mac", { mac: c.was_mac }), now: c.now_mac ? t("owner.mac", { mac: c.now_mac }) : t("owner.untold") }));
        }
      }
    }
    // A host the check found to be its recorded instrument again has nothing to say.
    for (const r of [...ownerChanged.keys()]) {
      if (interfaceOf(r).group === "lan" && !unconfirmedHosts.has(lanHost(r)) && !ownerChanged.get(r).idn) ownerChanged.delete(r);
    }
    if (st.moved.length) {
      toast(st.moved.map(([from, to]) => t("units.moved", { from, to })).join(" "));
      settings = settingsMap(await invoke("instrument_settings"));
      await reloadAliases();
      await window.__TAURI__.event.emit("aliases-changed", {});
    }
  } finally {
    checkingUnits = false;
  }
}

// ---- a LAN address that has changed owner ---------------------------------------------------------

// The LAN instruments whose address answered with another unit than the one recorded there (lanunits.rs), by the item's
// resource: {was, settings, aliases, idn, via}, until the user settles it on the instrument's page. Nothing is changed
// until then, and VISA sees what it saw.
const ownerChanged = new Map();
const ownerSaid = new Set(); // said in the status bar once a run
function unitName(u) { return [u.vendor, u.model].filter(Boolean).join(" ") + (u.serial ? " (" + u.serial + ")" : ""); }
async function checkOwner(r, via, text) {
  // LAN by host, GPIB and serial by their own names (a USB name is already the unit).
  if (!["lan", "gpib", "serial"].includes(interfaceOf(r).group)) return;
  let c;
  try { c = await invoke("lan_unit_seen", { resource: via, idn: text }); } catch (e) { return; }
  if (!c) {
    // The answer is the unit recorded: an *IDN? notice is over. One the MAC raised stays until the MAC is right again (checkUnits): the
    // same unit with another MAC (a new network card) is the user's to keep.
    const o = ownerChanged.get(r);
    if (o && !o.wasMac) {
      ownerChanged.delete(r);
      render();
    } else if (o) {
      o.idn = text;
    }
    return;
  }
  ownerChanged.set(r, { ...ownerChanged.get(r), ...c, idn: text, via });
  if (!ownerSaid.has(r)) {
    ownerSaid.add(r);
    toast(t("owner.toast", { host: lanHost(r), was: unitName(c.was), now: unitName(idnParts(text)) }));
  }
  render();
}
// The same unit as lanunits.rs says it: manufacturer and model, and the serial when both have one that tells units apart.
function sameUnit(a, b) {
  const n = s => (s || "").split(/\s+/).filter(Boolean).join(" ").toLowerCase();
  const usable = s => { const x = (s || "").trim(); return x.length > 0 && ![...x].every(c => c === x[0]); };
  return n(a.vendor) === n(b.vendor) && n(a.model) === n(b.model) && (!(usable(a.serial) && usable(b.serial)) || n(a.serial) === n(b.serial));
}
// Where the unit an address was recorded for answers now, in this tree (another plain LAN instrument's *IDN? answer); null if nowhere.
function ownerNowAt(r, was) {
  if (isGatewayDevice(r)) return null;
  return allInstruments(model()).find(it => it.resource !== r && interfaceOf(it.resource).group === "lan" && !isGatewayDevice(it.resource)
    && lanHost(it.resource) !== lanHost(r) && idn.get(it.resource) && idn.get(it.resource).text && sameUnit(idnParts(idn.get(it.resource).text), was)) || null;
}
function keptLine(x) {
  const what = [x.expose ? t("owner.kept_expose", { how: t("expose." + x.expose) }) : null, x.idn ? null : t("owner.kept_idn_off"),
    x.hidden ? t("owner.kept_hidden") : null, x.name ? t("owner.kept_name", { name: x.name }) : null].filter(Boolean).join(" · ");
  return el("li", {}, el("span", { class: "mono" }, x.resource), " — ", what);
}
function keptAlias(a) {
  return el("li", {}, el("span", { class: "mono" }, a.resource), " — ",
    t(a.bound ? "owner.kept_alias_bound" : "owner.kept_alias", { alias: a.alias }) + (a.group ? " " + t("owner.kept_alias_group") : ""));
}
async function settleOwner(r, how, host) {
  const c = ownerChanged.get(r);
  if (!c) return;
  const now = unitName(idnParts(c.idn));
  if (how === "move") {
    // The same names at the new host: only the host changes (a ",port" after it stays).
    const at = x => { const p = resParts(x); p[1] = p[1].replace(/^(\[[^\]]*\]|[^,]+)/, host); return p.join("::"); };
    const there = c.settings.map(x => at(x.resource)).filter(x => settings.has(x));
    const body = there.length ? [t("owner.move_confirm_text", { host }), el("ul", { class: "mono" }, there.map(x => el("li", {}, x)))] : [t("owner.move_confirm_none", { host })];
    if (!await confirmDialog(t("owner.move_confirm_title", { host }), body, t("owner.move", { host }), false)) return;
  }
  await act("owner:" + r, async () => {
    await invoke("lan_unit_settle", { resource: c.via, idn: c.idn, how, host: host || null });
    ownerChanged.delete(r);
    await checkUnits();
    // A move records the old unit at its new address: that address's own notice, if it had one, may be settled by it.
    for (const [x, o] of [...ownerChanged]) await checkOwner(x, o.via, o.idn);
    settings = settingsMap(await invoke("instrument_settings"));
    await reloadAliases();
    await window.__TAURI__.event.emit("aliases-changed", {});
  }, () => t("owner.done_" + how, { host: host || lanHost(r), now }), true);
}
// The notice at the top of the instrument's page, while its address keeps what the last instrument there left.
function ownerBox(r) {
  const c = ownerChanged.get(r);
  if (!c) return null;
  const key = "owner:" + r;
  // The MAC changed and *IDN? says it is the same unit (a new network card, say): its own aliases are no leftovers, and there is
  // nowhere else to move to.
  const same = !!(c.wasMac && c.idn && c.was && c.was.model && sameUnit(idnParts(c.idn), c.was));
  const there = same ? null : ownerNowAt(r, c.was);
  const host = there ? lanHost(there.resource) : null;
  const was = c.was && c.was.model ? unitName(c.was) : t("owner.mac", { mac: c.wasMac || "?" });
  const now = c.idn ? unitName(idnParts(c.idn)) : c.nowMac ? t("owner.mac", { mac: c.nowMac }) : t("owner.untold");
  const items = [...c.settings.map(keptLine), ...c.aliases.filter(a => !(same && a.bound)).map(keptAlias)];
  const button = (how, label, hint, primary) => el("button", { type: "button", class: primary ? "primary" : null, title: hint, disabled: busy.has(key), onclick: () => settleOwner(r, how, host) }, label);
  return el("section", { class: "box owner-box", role: "alert" },
    el("div", { class: "bar" }, el("h2", {}, t(same ? "owner.title_same_unit" : "owner.title"))),
    el("div", { class: "pad" },
      el("div", {}, same
        ? t("owner.text_same_unit", { host: lanHost(r), unit: unitName(c.was), was: c.wasMac, now: c.nowMac || "?" })
        : t("owner.text", { host: lanHost(r), was, now })),
      items.length ? el("ul", { class: "owner-kept" }, items) : el("div", { class: "hint" }, t("owner.nothing_kept")),
      el("div", { class: "row" },
        there ? button("move", t("owner.move", { host }), t("owner.move_hint", { was, host }), true) : null,
        button("keep", t("owner.keep"), t("owner.keep_hint", { now }), same),
        button("clear", t("owner.clear"), t("owner.clear_hint"), !there && !same)),
      there || same ? null : el("div", { class: "hint" }, t("owner.not_seen", { was }))));
}

// ---- the tree ----------------------------------------------------------------------------------

function isSelected(s) {
  return selected.kind === s.kind && selected.id === s.id;
}
let reveal = false; // bring the selected row into the tree's view on the next render

function select(s) {
  selected = s;
  reveal = true;
  for (const k of [...notes.keys()]) if (!busy.has(k)) notes.delete(k);
  // Whether the service runs changes by itself (it exits when idle): asked again each time it is shown.
  extra.delete("service");
  extra.delete("share");
  render();
  document.getElementById("detail").scrollTop = 0;
}

// A row of the tree. A page that is not an instrument (sharing, the service) has an icon in front instead, and its dot after.
// What a heading (a kind of instrument, an adapter, a gateway) has on its right: what names it (an adapter's board, a gateway's
// address), then how many instruments, the latter only with tree_counts on (features.conf).
// What the adapter says it is ("Viper488 Classic"), else its make.
function adapterName(b) {
  return b.model || t("kind." + b.kind);
}
function headingWhere(name, n) {
  const count = features.tree_counts ? t("count.instruments", { n }) : null;
  return [name, count].filter(Boolean).join(" · ") || null;
}
function node(sel, dot, title, sub, where, child, mono, glyph) {
  return el("button", { type: "button", class: "node" + (child ? " child" : "") + (glyph ? " page" : ""), "data-node": sel.kind + ":" + (sel.id || ""), "aria-current": isSelected(sel) ? "true" : null, onclick: () => select(sel) },
    // A row with a mark and a state (a GPIB board's heading) has the state's dot on the mark's corner.
    glyph && dot !== "none" ? el("span", { class: "glyph" }, icon(glyph, 16), el("span", { class: "dot badge " + dot }))
      : glyph ? icon(glyph, 16) : el("span", { class: "dot " + dot }),
    el("span", { class: "label" },
      el("span", { class: child ? "name" : "title" }, title),
      sub ? el("span", { class: "sub" }, sub) : null),
    where ? el("span", { class: "where" + (mono ? " mono" : "") }, where) : null);
}

// A tree row is narrow: the user's name or the model, with the manufacturer under it.
function instrumentNode(it, short, plainName) {
  const a = idn.get(it.resource);
  const p = a && a.text ? idnParts(a.text) : null;
  const name = settingsOf(it.resource).name;
  const title = name || (p && (p.model || a.text)) || null;
  let dot = it.online ? "ok" : "idle";
  let sub = !name && p && p.model ? p.vendor : null;
  if (!it.online) sub = t("instrument.offline");
  else if (a && a.pending && !title) sub = t("instrument.asking");
  else if (a && a.error) { dot = "warn"; sub = t("instrument.no_answer"); }
  else if (a && a.inUse && !title) sub = t("instrument.in_use");
  else if (!title) sub = settingsOf(it.resource).idn || interfaceOf(it.resource).group === "serial" ? null : t("instrument.idn_off");
  // A LAN instrument not yet named goes by its host: it stands for several resource names.
  const plain = plainName || (it.members ? lanShort(it.resource) : it.resource);
  const n = node({ kind: "instrument", id: it.resource }, dot, title || plain, sub, title ? short(it.resource) : null, true, true);
  n.dataset.resource = it.resource;
  if (it.hidden) { n.classList.add("hidden-item"); n.title = t("instrument.hidden"); }
  if (ownerChanged.has(it.resource)) {
    const label = n.querySelector(".label > span");
    if (label) label.append(el("span", { class: "owner-flag", title: t("owner.title"), "aria-label": t("owner.title") }, "!"));
  }
  return n;
}

function shown(list) { return showHidden ? list : list.filter(it => !it.hidden); }

function renderTree(m) {
  const tree = document.getElementById("tree");
  const list = document.getElementById("tree-list");
  // Rebuilt whenever an answer arrives: where the user had scrolled to stays put.
  const top = list.scrollTop;
  // The rows are new each time: the one that had the keyboard's focus gets it back.
  const f = document.activeElement;
  const focused = f && tree.contains(f) && f.dataset.node;
  fillTree(list, m);
  fillFoot(document.getElementById("tree-foot"), m);
  list.scrollTop = top;
  if (focused) {
    const again = tree.querySelector('[data-node="' + CSS.escape(focused) + '"]');
    if (again) again.focus({ preventScroll: true });
  }
  // A selection made elsewhere (a list on the right, the menu, --page): its row comes into view.
  const current = reveal && tree.querySelector('[aria-current="true"]');
  if (current) { current.scrollIntoView({ block: "nearest" }); reveal = false; }
}

function fillTree(tree, m) {
  tree.replaceChildren();
  if (snap === null) { tree.append(el("div", { class: "empty loading" }, el("span", { class: "spinner small", "aria-hidden": "true" }), t("tree.loading"))); return; }
  if (snap.stopped) {
    tree.append(el("div", { class: "empty stopped" }, el("div", {}, snap.error), el("div", {}, t("service.stopped_tree_hint"))));
    return;
  }
  if (snap.error) { tree.append(el("div", { class: "empty error" }, snap.error)); return; }

  // Each GPIB board is a group of its own, beside USB, LAN and the rest: its adapter's make under its name, its
  // instruments one level in. Without a board (none plugged in or remembered, or the service's error), one GPIB group
  // says so.
  if (snap.boards.Err || !m.boards.length) {
    tree.append(el("div", { class: "group" },
      node({ kind: "group", id: "gpib" }, "none", t("group.gpib"), null, null, false, false, GROUP_ICONS.gpib),
      el("div", { class: "empty" + (snap.boards.Err ? " error" : "") }, snap.boards.Err ? t("service.error", { error: snap.boards.Err }) : t("boards.none"))));
  }
  for (const b of m.boards) {
    // One line, as the other kinds: the adapter's make, its board on the right.
    const head = node({ kind: "board", id: b.board }, b.present ? "ok" : "idle", b.kind ? adapterName(b) : b.board, null,
      headingWhere(b.kind ? b.board : null, shown(b.instruments).length), false, true, GROUP_ICONS.gpib);
    head.dataset.board = b.board;
    const box = el("div", { class: "group" }, head);
    for (const it of shown(b.instruments)) box.append(instrumentNode(it, gpibShort));
    tree.append(box);
  }

  const groups = [
    ["usb", "group.usb", r => r.split("::")[3] || r],
    ["lan", "group.lan", lanShort],
    ["serial", "group.serial", r => r.replace("::INSTR", "")],
    ["other", "group.other", r => r],
  ];
  for (const [g, key, short] of groups) {
    if (g === "other" && !m.other.length) continue;
    // The LAN group has the instruments that stand alone; a gateway's are in its own group, after it.
    const own = shown(m[g]).filter(it => !(g === "lan" && isGatewayDevice(it.resource)));
    const box = el("div", { class: "group" },
      node({ kind: "group", id: g }, "none", t(key), null, headingWhere(null, own.length), false, false, GROUP_ICONS[g]));
    for (const it of own) box.append(instrumentNode(it, short));
    tree.append(box);
    // Each LAN/GPIB gateway is a group of its own, as a GPIB board is: its state on its mark's corner, its instruments one level in.
    if (g === "lan") {
      for (const gw of m.gateways) {
        const gn = node({ kind: "gateway", id: gw.ip }, gw.online ? "ok" : "idle", t("lan.gateway_short"), null,
          headingWhere(gw.ip, shown(gw.instruments).length), false, true, GROUP_ICONS.gateway);
        gn.dataset.gateway = gw.ip;
        const gbox = el("div", { class: "group" }, gn);
        for (const it of shown(gw.instruments)) gbox.append(instrumentNode(it, gatewayShort, gatewayShort(it.resource)));
        tree.append(gbox);
      }
    }
  }

  // What other computers of the sharing group share (asked of the registry once per refresh; nothing is shown
  // for a computer that has not joined a group).
  const others = sharedByOthers();
  askShared(others);
  const listed = others.filter(x => showHidden || !sharedViewOf(x.alias).hidden);
  if (others.length) {
    const box = el("div", { class: "group" },
      node({ kind: "group", id: "shared" }, "none", t("group.shared"), null, headingWhere(null, listed.length), false, false, GROUP_ICONS.shared));
    for (const x of listed) {
      const n = node({ kind: "shared", id: x.alias }, { true: "ok", false: "idle" }[sharedPresent(x)] || "none", x.alias,
        t("shared.owner", { owner: x.owner }), null, true, true);
      if (sharedViewOf(x.alias).hidden) { n.classList.add("hidden-item"); n.title = t("instrument.hidden"); }
      box.append(n);
    }
    tree.append(box);
  }
}

// Under the tree, where scrolling does not take it: the switch for hidden instruments.
function fillFoot(foot, m) {
  foot.replaceChildren();
  const hidden = allInstruments(m).filter(it => it.hidden).length + sharedByOthers().filter(x => sharedViewOf(x.alias).hidden).length;
  if (hidden) {
    const box = el("input", { type: "checkbox", checked: showHidden });
    box.addEventListener("change", () => { showHidden = box.checked; store("tree.showHidden", showHidden ? "1" : ""); render(); });
    foot.append(el("label", { class: "switch tree-hidden" }, box, el("span", {}, t("tree.show_hidden", { n: hidden }))));
  }
}
// Each group's heading has its own mark: a GPIB board its cable's connector, thumbscrews either side, a LAN/GPIB
// gateway that connector joined to a node on a network line, the USB trident, a network, a serial connector, a box for the rest,
// and a screen for what other computers share.
const GROUP_ICONS = {
  gpib: ["M4 5h16v7a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1z", "M8 8h8l-.8 2H8.8z", "M2 8.5h2", "M20 8.5h2", "M9 13v3h6v-3", "M12 16v6"],
  usb: ["M12 2v15", "M9 5l3-3 3 3", "M12 13l-5-3V7", "M12 15l5-3V9", "rect x=5.5 y=5 width=3 height=2", "circle cx=17 cy=8 r=1.2", "circle cx=12 cy=19.5 r=2"],
  gateway: ["M4 3h16v6a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1z", "M8 5.5h8l-.8 2H8.8z", "M12 10v4", "rect x=8 y=14 width=8 height=6 rx=1", "M2 17h6", "M16 17h6"],
  lan: ["rect x=9 y=2 width=6 height=6 rx=1", "rect x=2 y=16 width=6 height=6 rx=1", "rect x=16 y=16 width=6 height=6 rx=1", "M12 8v4", "M5 16v-4h14v4"],
  serial: ["M2.5 7h19l-2.2 10H4.7z", "M8 10.5h.01", "M12 10.5h.01", "M16 10.5h.01", "M10 13.5h.01", "M14 13.5h.01"],
  other: ["M21 8l-9-5-9 5v8l9 5 9-5z", "M3 8l9 5 9-5", "M12 13v8"],
  shared: ["rect x=2 y=3 width=20 height=14 rx=2", "M8 21h8", "M12 17v4"],
};
// How a GPIB instrument and one behind a LAN/GPIB gateway are shown under their board or gateway: by address.
function gpibShort(r) { const x = /::(\d+(?:::\d+)?)::INSTR$/.exec(r); return x ? t("gpib.address", { a: x[1] }) : r; }
function gatewayShort(r) { return resParts(r)[2] || r; }

// ---- the right side ----------------------------------------------------------------------------

// A page's heading: what kind of thing (none for a page of its own), its name, its state, and the page's main action.
function head(kicker, title, pill, action) {
  return el("div", { class: "head" },
    el("div", {}, kicker ? el("div", { class: "kicker" }, kicker) : null, el("h1", {}, title)),
    el("div", { class: "head-end" }, pill ? el("span", { class: "pill " + pill[0] }, pill[1]) : null, action || null));
}
// Short facts as "label: value" lines: [label, value, mono] each, null for one that does not apply.
function facts(...rows) {
  return el("dl", { class: "ident-list" }, rows.filter(Boolean).flatMap(([k, v, mono]) => [el("dt", {}, k), el("dd", { class: mono ? "mono" : null }, v || "—")]));
}
const REFRESH = ["M21 12a9 9 0 1 1-3-6.7", "M21 4v5h-5"];
// One line of the identification: "label: value", the label column as wide as its longest label.
function identRow(k, v) {
  // Every value in the monospaced face, so that the dash for nothing to show is the same on every line too.
  return [el("dt", {}, k), el("dd", { class: "mono" }, v || "—")];
}
function tip(b, text) {
  const h = b.querySelector(".bar h2");
  const w = el("div", { class: "bar-title" });
  h.replaceWith(w);
  w.append(h, helpMark(text));
  return b;
}
// What a row without a name says: not found when it does not answer (not "unnamed": nothing could be asked), else why
// it has no name yet.
function untitled(it) {
  if (!it.online) return t("instrument.offline");
  return t(interfaceOf(it.resource).group === "serial" ? "instrument.not_asked" : "instrument.unnamed");
}
function listRow(it, short) {
  const title = titleOf(it.resource);
  return el("button", { type: "button", class: "listrow" + (it.hidden ? " hidden-item" : ""), "data-resource": it.resource, onclick: () => select({ kind: "instrument", id: it.resource }) },
    el("span", { class: "dot " + (it.online ? "ok" : "idle") }),
    el("span", { class: "where mono" }, short(it.resource)),
    el("span", { class: "label" }, title || untitled(it)));
}

// ---- the console ---------------------------------------------------------------------------------

// The console is a window of its own (console.html, console.js), one for each instrument: Rust makes it, and
// opening one that is open brings it forward.
async function openConsole(r) {
  const it = itemOf(model(), r);
  const members = (it && it.members ? it.members : [{ resource: r, online: it ? it.online : false }])
    .map(x => ({ resource: x.resource, online: !!x.online, proto: t("proto." + lanProto(x.resource).proto) }));
  try { await invoke("open_console", { resource: r, members, via: it && it.via ? it.via : r, title: t("console.window_title", { name: r }) }); }
  catch (e) { toast(String(e), true); }
}

// ---- the instrument pane ---------------------------------------------------------------------------

function instrumentPane(m, r) {
  const it = itemOf(m, r);
  if (!it) return [head(t("instrument.kicker"), r, ["idle", t("instrument.gone")])];
  const s = settingsOf(r);
  const a = idn.get(r);
  const p = a && a.text ? idnParts(a.text) : null;
  const group = interfaceOf(r).group;
  let answer = t("instrument.idn_not_asked");
  if (a && a.pending) answer = t("instrument.asking");
  else if (a && a.error) answer = t("instrument.idn_error", { error: a.error });
  else if (a && a.inUse) answer = t("instrument.idn_in_use");
  else if (a && a.text) answer = a.text;

  const name = field("name:" + r, s.name || "", { id: "name-field", placeholder: titleOf(r) && !s.name ? titleOf(r) : t("settings.name_placeholder") });
  const ask = el("input", { type: "checkbox", checked: s.idn });
  const hide = el("input", { type: "checkbox", checked: s.hidden });
  const save = async () => {
    const err = await saveSettings(r, { idn: ask.checked, name: name.value.trim() || null, hidden: hide.checked });
    if (!err) drafts.delete("name:" + r);
    toast(err || t("settings.saved"), !!err);
    render();
  };
  savesItself(name, () => s.name || "", save);
  ask.addEventListener("change", save);
  hide.addEventListener("change", save);

  return [
    // No state here: the tree's dot and line under the name say it.
    head(t("group." + group), titleOf(r) || (it.members ? lanShort(r) : r), null),
    ownerBox(r),
    box(t("instrument.idn"),
      el("div", { class: "bar-actions" },
        el("button", { type: "button", class: "iconbtn", title: t("action.identify"), "aria-label": t("action.identify"),
          disabled: !it.online || (a && a.pending), onclick: () => identify(r) },
          icon(REFRESH)),
        el("button", { type: "button", class: "iconbtn", title: t("action.open_console"), "aria-label": t("action.open_console"),
          onclick: () => openConsole(r) },
          icon(CONSOLE_ICON)),
        webButton(it)),
      // The list and the switches take the left of the panel; the right is for the picture (pictures.js), if the user has
      // given one, fitted to the left's height so a tall one neither grows the panel nor moves the switches.
      el("div", { class: "ident" },
        el("div", { class: "ident-main" },
          el("dl", { class: "ident-list" },
            identRow(t("field.vendor"), p && p.vendor),
            identRow(t("field.model"), p && p.model),
            identRow(t("field.serial"), p && p.serial),
            identRow(t("field.firmware"), p && p.firmware)),
          // The raw answer is these four fields; without one, why there is none (asking, not asked, the error) is not.
          a && a.text ? null : el("div", { class: "hint" }, answer),
          // Under a line, still in the left column: what this window does with the instrument.
          el("div", { class: "ident-opts" },
            el("label", { class: "switch", title: t("settings.idn_hint") }, ask, el("span", {}, t("settings.idn"))),
            el("label", { class: "switch", title: t("settings.hidden_hint") }, hide, el("span", {}, t("settings.hidden"))))),
        pictureArt(it))),
    namesBox(it, name),
  ];
}
const CONSOLE_ICON = ["M4 17l6-6-6-6", "M12 19h8"];
const GLOBE = ["circle cx=12 cy=12 r=9", "M3 12h18", "M12 3a14 14 0 0 1 0 18", "M12 3a14 14 0 0 0 0 18"];
// A network instrument's web page (LXI's, on port 80): asked once per host while it answers, and a button that opens it
// in the system's browser when there is one. Not a gateway's GPIB instrument: the page there is the gateway's.
function webButton(it) {
  if (interfaceOf(it.resource).group !== "lan" || isGatewayDevice(it.resource) || !it.online) return null;
  // Bare: web_page takes an IPv6 address without its brackets.
  const host = lanHost(it.resource).replace(/^\[(.*)\]$/, "$1");
  const url = (need("web:" + host, "web_page", { host }) || {}).value;
  if (!url) return null;
  return el("button", { type: "button", class: "iconbtn", title: t("action.open_web", { url }), "aria-label": t("action.open_web", { url }),
    onclick: () => invoke("open_web", { url }).catch(e => toast(String(e), true)) }, icon(GLOBE));
}

// A text field that saves itself on Enter and on leaving it, when its text is not what is saved (`saved()`); not when it
// is only taken away by a re-render of the page (renderDetail), which puts the text back in a new one.
// Left by a click: the save (which redraws the page) waits until that click is done, or the button clicked would be
// replaced under the pointer and the click lost.
let rendering = false;
let pressed = false;
const afterPress = [];
document.addEventListener("pointerdown", () => { pressed = true; }, true);
document.addEventListener("pointerup", () => setTimeout(() => { pressed = false; for (const f of afterPress.splice(0)) f(); }, 0), true);
function savesItself(input, saved, save) {
  input.addEventListener("keydown", e => { if (e.key === "Enter") save(); });
  input.addEventListener("blur", () => {
    if (rendering || !input.isConnected || input.value.trim() === saved()) return;
    if (pressed) afterPress.push(save); else save();
  });
}

// The aliases of one instrument (aliases.conf, which every program's viOpen takes): a row for each resource name it
// answers to (an instrument on a network has several: inst0, hislip0, a socket), with its alias, or "-" where it has
// none (a name with more than one alias has a row for each), the name and the alias each with a copy button, and how
// the name is listed to applications. Nothing is changed here: aliases are added, changed and removed, and how they are
// listed is set, in the aliases window. Above the rows, this window's own name for the instrument, when that is shown.
function aliasesOf(r) {
  // The group's first: on a computer in a group a network instrument's alias is the group's (share::names).
  return aliasList.filter(a => a.resource.toLowerCase() === r.toLowerCase()).sort((a, b) => b.group - a.group);
}
function namesBox(it, name) {
  const resources = it.members && it.members.length ? it.members : [{ resource: it.resource, online: it.online }];
  const proto = resources.length > 1;
  const rows = resources.flatMap(x => (aliasesOf(x.resource).length ? aliasesOf(x.resource) : [null]).map(a => el("tr", {},
    el("td", {}, el("span", { class: "cell" },
      el("span", { class: "dot " + (x.online ? "ok" : "idle"), title: t(x.online ? "instrument.online" : "instrument.offline") }),
      proto ? el("span", { class: "proto" }, t("proto." + lanProto(x.resource).proto)) : null,
      el("span", { class: "mono" }, x.resource), copyButton(x.resource))),
    a ? el("td", {}, el("span", { class: "cell" }, el("span", { class: "mono" }, a.alias), lockMark(a.alias), copyButton(a.alias)),
      a.group ? el("div", { class: "sub" }, t("alias.group")) : null)
      : el("td", {}, el("span", { class: "mono none", title: t("names.no_alias") }, "-")),
    el("td", { class: "listed" }, t("expose." + listedAs(x.resource))))));
  const st = extra.get("shared_tree");
  const groupWide = resources.some(x => interfaceOf(x.resource).group === "lan") && !!(st && st.value && st.value.joined);
  return tip(box(t("names.title"), null,
    features.display_name ? el("div", { class: "form names" }, el("label", { for: "name-field", class: "k" }, t("settings.name")), name) : null,
    el("table", { class: "alias-rows" },
      el("thead", {}, el("tr", {}, el("th", {}, t("alias.resource")), el("th", {}, t("names.alias")), el("th", {}, t("names.expose")))),
      el("tbody", {}, rows)),
    groupWide ? el("div", { class: "hint" }, t("names.group_wide")) : null), t("names.hint"));
}

// How applications listing resources see `r`: what it is set to, or the default, alias only; without an alias, the
// choices that name it come to the name.
function listedAs(r) {
  const w = settingsOf(r).expose || "alias";
  return aliasesOf(r).length || w === "none" ? w : "name";
}
// The unit an alias is bound to (identities.conf, and the group's), if it is. It is bound and unbound in the aliases
// window; here a lock beside the alias says that it is, with alias_binding off too: a bound alias is still checked and
// looked for when opened, and one that does not open must not hide why.
function unitOf(alias) {
  return identityList.find(u => u.alias.toLowerCase() === alias.toLowerCase());
}
const LOCK_ICON = ["rect x=5 y=11 width=14 height=10 rx=2", "M8 11V7a4 4 0 0 1 8 0v4"];
function lockMark(alias) {
  const u = unitOf(alias);
  if (!u) return null;
  const text = t("unit.bound", { vendor: u.vendor, model: u.model, serial: u.serial });
  return el("span", { class: "lock", title: text, role: "img", "aria-label": text }, icon(LOCK_ICON));
}


function boardPane(m, id) {
  const b = m.boards.find(x => x.board === id);
  if (!b) return [head(t("group.gpib"), id, ["idle", t("state.absent")])];
  const addr = r => (/::(\d+(?:::\d+)?)::INSTR$/.exec(r) || [, r])[1];
  return [
    head(t("group.gpib"), b.board, b.present ? ["ok", t("state.attached")] : ["idle", t("state.absent")]),
    box(t("board.adapter"), null, facts([t("field.adapter"), b.kind ? adapterName(b) : null], [t("field.serial"), b.serial, true])),
    box(t("board.instruments", { n: b.instruments.length }), null,
      b.instruments.length ? el("div", {}, b.instruments.map(it => listRow(it, addr)))
        : el("div", { class: "hint" }, t(b.present ? "board.empty" : "board.absent_hint"))),
    speedBox(b),
    renBox(b),
  ];
}

// Whether the adapter holds REN asserted (ren.conf); released by default, so that nothing is put in Remote by being looked at.
function renBox(b) {
  const key = "ren:" + b.board;
  if (!b.kind) return null;
  const cb = el("input", { type: "checkbox", checked: !!b.ren, disabled: busy.has(key) || !b.present || !b.ren_settable });
  cb.addEventListener("change", () => act(key, async () => {
    await invoke("set_ren", { board: Number(b.board.slice(4)), on: cb.checked });
    b.ren = cb.checked;
    const x = snap.boards.Ok.find(y => y.board === b.board);
    if (x) x.ren = cb.checked;
    return cb.checked;
  }, on => t(on ? "ren.set_on" : "ren.set_off")));
  return tip(box(t("ren.title"), null,
    switchRow(cb, t("ren.hold"), t("ren.short")),
    noteEl(key),
    b.present ? null : el("div", { class: "hint" }, t("ren.absent"))), t("ren.hint"));
}

function speedBox(b) {
  const key = "speed:" + b.board;
  if (!b.kind || !b.speed_settable) return box(t("speed.title"), null, el("div", { class: "hint" }, t("speed.unsupported")));
  const current = b.speed || "normal";
  const sel = el("select", { "aria-label": t("speed.title"), disabled: busy.has(key) || !b.present });
  for (const l of ["low", "normal", "high"]) sel.append(new Option(t("speed." + l), l, false, l === current));
  sel.addEventListener("change", () => act(key, async () => {
    const level = await invoke("set_speed", { board: Number(b.board.slice(4)), level: sel.value });
    b.speed = level;
    const x = snap.boards.Ok.find(y => y.board === b.board);
    if (x) x.speed = level;
    return level;
  }, l => t("speed.set", { level: t("speed." + l) })));
  const sb = box(t("speed.title"), null,
    el("div", { class: "row" }, sel, noteEl(key)),
    b.present ? null : el("div", { class: "hint" }, t("speed.absent")));
  return b.present ? tip(sb, t("speed.hint")) : sb;
}

function groupPane(m, g) {
  if (g === "gpib") {
    return [
      head(t("group.kicker"), t("group.gpib"), null),
      box(t("count.adapters", { n: m.boards.length }), null,
        m.boards.length ? el("div", {}, m.boards.map(b => el("button", { type: "button", class: "listrow", onclick: () => select({ kind: "board", id: b.board }) },
          el("span", { class: "dot " + (b.present ? "ok" : "idle") }),
          el("span", { class: "where mono" }, b.board),
          el("span", { class: "label" }, el("span", {}, b.kind ? adapterName(b) : "—"),
            el("span", { class: "sub" }, b.present ? t("count.instruments", { n: b.instruments.length }) : t("state.absent"))))))
          : el("div", { class: "hint" }, t("boards.none"))),
    ];
  }
  if (g === "shared") {
    const others = sharedByOthers();
    const dot = x => ({ true: "ok", false: "idle" })[sharedPresent(x)] || "none";
    return [
      head(t("group.kicker"), t("group.shared"), null),
      tip(box(t("count.instruments", { n: others.length }), null,
        others.length ? el("div", {}, others.map(x => el("button", { type: "button", class: "listrow", onclick: () => select({ kind: "shared", id: x.alias }) },
          el("span", { class: "dot " + dot(x) }),
          el("span", { class: "label" }, el("span", {}, x.alias), el("span", { class: "sub mono" }, resName(x.resource))),
          el("span", { class: "muted" }, t("shared.owner", { owner: x.owner })))))
          : el("div", { class: "hint" }, t("share.others_none"))), t("share.others_hint")),
    ];
  }
  const list = m[g] || [];
  const short = g === "lan" ? lanShort : g === "usb" ? r => r.split("::")[3] || r : r => r.replace("::INSTR", "");
  const parts = [head(t("group.kicker"), t("group." + g), null)];
  // The LAN instruments and the serial ports are one list each, with what can be done to each line in it.
  if (g === "lan") return [...parts, ...lanBoxes(m)];
  if (g === "serial") return [...parts, serialBox(m)];
  parts.push(box(t("count.instruments", { n: list.length }), null,
    list.length ? el("div", {}, list.map(it => listRow(it, short)))
      : el("div", { class: "hint" }, t("group.empty." + g))));
  if (g === "usb") parts.push(usbBox());
  return parts;
}

// A line of a list that opens an instrument's page from its name, with buttons of its own after it.
function openLine(it, where, sub, ...after) {
  const title = titleOf(it.resource);
  return el("div", { class: "listrow static" + (it.hidden ? " hidden-item" : ""), "data-resource": it.resource },
    el("span", { class: "dot " + (it.online ? "ok" : "idle") }),
    el("button", { type: "button", class: "linkrow", onclick: () => select({ kind: "instrument", id: it.resource }) },
      el("span", { class: "where mono" }, where),
      el("span", { class: "label" },
        el("span", {}, title || untitled(it)),
        sub ? el("span", { class: "sub" }, sub) : null)),
    ...after);
}

function loading(x) {
  if (!x || x.loading) return el("div", { class: "hint" }, t("state.loading"));
  if (x.error) return el("div", { class: "error" }, x.error);
  return null;
}

// The gateways' finds, by address: {loading} | {value: [names]} | {error}.
const gwFound = new Map();

// One gateway's "find its instruments" and what it found; `row` draws a found name.
function gatewayBlock(g, row, hint = true) {
  const f = gwFound.get(g.ip);
  return el("div", { class: "rows" },
    el("div", { class: "listrow static" },
      el("span", { class: "label" }, el("span", { class: "mono" }, g.ip), el("span", { class: "sub" }, t("lan.gateway", { address: g.address }))),
      el("button", { type: "button", disabled: !!(f && f.loading), onclick: async () => {
        gwFound.set(g.ip, { loading: true }); render();
        try { gwFound.set(g.ip, { value: await invoke("lan_gateway_find", { ip: g.ip }) }); }
        catch (e) { gwFound.set(g.ip, { error: String(e) === "unsupported" ? t("lan.gateway_unsupported") : String(e) }); }
        render();
      } }, t("lan.gateway_find"))),
    hint ? el("div", { class: "hint" }, t("lan.gateway_hint")) : null,
    f ? (f.loading ? el("div", { class: "hint" }, t("state.loading"))
      : f.error ? el("div", { class: "error" }, f.error)
      : f.value.length ? f.value.map(row) : el("div", { class: "hint" }, t("lan.gateway_none"))) : null);
}

// A gateway's page: its instruments, and the button that looks for more.
function gatewayPane(m, ip) {
  const gw = m.gateways.find(x => x.ip === ip);
  if (!gw) return [head(t("group.kicker"), ip, null), el("div", { class: "hint" }, t("lan.gateway_gone"))];
  const listed = new Set(m.lan.map(x => x.resource));
  const add = name => act("lan:list", async () => {
    const [canonical, added] = await invoke("lan_add", { resource: name });
    extra.delete("lan_check");
    await reloadSnapshot();
    return [canonical, added];
  }, ([n, added]) => t(added ? "lan.added" : "lan.already", { name: n }));
  const row = n => el("div", { class: "listrow static" },
    el("span", { class: "label mono" }, n),
    listed.has(n) ? el("span", { class: "muted" }, t("lan.listed"))
      : el("button", { type: "button", disabled: busy.has("lan:list"), onclick: () => add(n) }, t("action.add")));
  return [
    head(t("lan.gateway_kicker"), ip, null),
    tip(box(t("count.instruments", { n: gw.instruments.length }), null,
      gw.instruments.length ? el("div", {}, gw.instruments.map(it => listRow(it, gatewayShort)))
        : el("div", { class: "hint" }, t("lan.gateway_empty")),
      noteEl("lan:list")), t("lan.gateway_address_hint")),
    tip(box(t("lan.gateway_find"), null, gatewayBlock({ ip, address: gw.address == null ? "?" : gw.address }, row, false)), t("lan.gateway_hint")),
  ];
}

function scanRows(result, names, add) {
  const row = n => el("div", { class: "listrow static" },
    el("span", { class: "label mono" }, n),
    names.has(n) ? el("span", { class: "muted" }, t("lan.listed"))
      : el("button", { type: "button", disabled: busy.has("lan:list"), onclick: () => add(n) }, t("action.add")));
  const gateways = result.gateways.map(g => gatewayBlock(g, row));
  if (!result.names.length && !gateways.length) return el("div", { class: "hint" }, t("lan.nothing"));
  return el("div", { class: "rows" }, ...gateways, ...result.names.map(row));
}

function lanBoxes(m) {
  const info = need("lan", "lan");
  const reach = need("lan_check", "lan_check");
  const scan = extra.get("lan_scan");
  const reloadLan = async () => { extra.delete("lan_check"); await reloadSnapshot(); };

  const discover = el("input", { type: "checkbox", checked: !!(info.value && info.value.discover), disabled: !info.value || busy.has("lan:discover") });
  discover.addEventListener("change", () => act("lan:discover", async () => {
    await invoke("lan_set_discover", { on: discover.checked });
    info.value.discover = discover.checked;
    await reloadSnapshot();
    return discover.checked;
  }, on => t(on ? "lan.discover_on" : "lan.discover_off")));

  const addField = field("lan:add", "", { class: "mono", placeholder: "TCPIP::192.168.1.20::hislip0::INSTR", "aria-label": t("lan.add_label"), spellcheck: "false" });
  const add = name => act("lan:list", async () => {
    const [canonical, added] = await invoke("lan_add", { resource: name });
    if (name === addField.value) drafts.delete("lan:add");
    await reloadLan();
    return [canonical, added];
  }, ([n, added]) => t(added ? "lan.added" : "lan.already", { name: n }));
  addField.addEventListener("keydown", e => { if (e.key === "Enter") add(addField.value); });

  // One line per instrument: a configured one says why it did not answer, and can be removed, as can one the group has an alias for;
  // one only discovered can be added, so that it is listed whether a scan finds it or not.
  const checked = new Map((reach.value || []).map(x => [x.name, x]));
  // As in the tree: a gateway's instruments are on its own page, not here.
  const own = m.lan.filter(it => !isGatewayDevice(it.resource));
  const rows = own.map(it => {
    const names = it.members ? it.members.map(x => x.resource) : [it.resource];
    const conf = names.filter(n => checked.has(n) || ((snap && snap.lan_configured) || []).includes(n));
    const silent = conf.map(n => checked.get(n)).find(x => x && !x.answered);
    // An instrument the group has an alias for is listed on every computer of the group: it needs no line here, and its Remove takes the alias.
    const grouped = removable([it]).aliases.length > 0;
    const sub = silent ? t("lan.no_answer", { ms: silent.ms, error: silent.error }) : conf.length ? null : grouped ? t("lan.group_alias") : t("lan.discovered");
    const action = conf.length || grouped
      ? el("button", { type: "button", "aria-label": t("action.remove_named", { name: lanShort(it.resource) }), disabled: busy.has("lan:list"), onclick: () => removeInstrument(it.resource) }, t("action.remove"))
      : el("button", { type: "button", disabled: busy.has("lan:list"), onclick: () => add(it.resource) }, t("action.add"));
    return openLine(it, lanShort(it.resource), sub, action);
  });
  const listed = new Set([...checked.keys(), ...((snap && snap.lan_configured) || [])]);
  return [
    tip(box(t("count.instruments", { n: own.length }),
      el("button", { type: "button", class: "iconbtn", title: t("action.check_again"), "aria-label": t("action.check_again"), disabled: reach.loading, onclick: () => { extra.delete("lan_check"); render(); } }, icon(REFRESH)),
      rows.length ? el("div", { class: "rows" }, rows) : el("div", { class: "hint" }, t("group.empty.lan")),
      el("div", { class: "row" }, addField, el("button", { type: "button", disabled: busy.has("lan:list"), onclick: () => add(addField.value) }, t("action.add"))),
      noteEl("lan:list")), t("lan.configured_hint", { path: info.value ? info.value.path || "" : "" })),
    tip(box(t("lan.scan"),
      el("button", { type: "button", disabled: scan && scan.loading, onclick: () => { extra.delete("lan_scan"); need("lan_scan", "lan_scan"); render(); } }, t("action.scan")),
      scan ? (loading(scan) || scanRows(scan.value, listed, add)) : null,
      switchRow(discover, t("lan.discover"), t("lan.discover_hint")),
      noteEl("lan:discover")), t("lan.scan_hint")),
  ];
}

// The serial ports, each with what it is (a serial instrument or a GPIB controller); a port's instrument opens from its line.
function serialBox(m) {
  const ports = need("serial", "serial_ports");
  const seen = new Set();
  const rows = (ports.value || []).map(p => {
    const key = "bridge:" + p.n;
    const sel = el("select", { "aria-label": t("serial.role", { port: p.name }), disabled: busy.has(key) });
    for (const x of ["port", "auto", "prologix", "ar488"]) sel.append(new Option(t("serial.as." + x), x, false, x === (p.bridge || "port")));
    sel.addEventListener("change", () => act(key, async () => {
      await invoke("set_bridge", { n: p.n, dialect: sel.value === "port" ? null : sel.value });
      extra.delete("serial");
      await reloadSnapshot();
      return sel.value;
    }, v => t(v === "port" ? "serial.now_port" : "serial.now_bridge", { port: p.name })));
    const it = !p.bridge && p.resource ? m.serial.find(x => x.resource === p.resource) : null;
    if (it) seen.add(it.resource);
    const sub = p.device ? (p.bridge ? t("serial.is_bridge") + " · " : "") + p.device : t("serial.unplugged");
    if (it) return openLine(it, p.name, sub, noteEl(key), sel);
    return el("div", { class: "listrow static" },
      el("span", { class: "dot " + (p.device ? "ok" : "idle") }),
      el("span", { class: "where mono" }, p.name),
      el("span", { class: "label" }, el("span", {}, p.bridge ? t("serial.is_bridge") : p.resource || ""), el("span", { class: "sub" }, sub)),
      noteEl(key), sel);
  });
  // A serial instrument whose port the list does not have (it went between the two answers): still there to open.
  for (const it of m.serial) if (ports.value && !seen.has(it.resource)) rows.push(openLine(it, it.resource.replace("::INSTR", ""), null));
  return tip(box(t("serial.ports"), null,
    loading(ports) || (rows.length ? el("div", { class: "rows" }, rows) : el("div", { class: "hint" }, t("group.empty.serial")))), t("serial.hint"));
}

function usbBox() {
  const dev = need("usb", "usb_devices");
  const rows = (dev.value || []).map(d => el("div", { class: "listrow static" },
    el("span", { class: "dot " + (d.not_ours ? "idle" : "ok") }),
    el("span", { class: "label" },
      el("span", {}, [d.manufacturer, d.product].filter(Boolean).join(" ") || d.resource),
      el("span", { class: "sub mono" }, d.resource),
      el("span", { class: "sub" }, d.not_ours ? t("usb.not_ours", { why: d.not_ours }) : t("usb.ours", { driver: d.driver })))));
  return tip(box(t("usb.devices"), null,
    loading(dev) || (rows.length ? el("div", { class: "rows" }, rows) : el("div", { class: "hint" }, t("usb.none")))), t("usb.hint"));
}

// The toolbar's Tools menu: the windows of their own (the aliases, the pictures, the I/O trace), then the sharing group's
// page and the service's, where the diagnostics bundle is made.
// The aliases window lists the instruments' resource names this window knows, with whether each answers (those with no alias
// too, to give one): it is told them when it opens and whenever they change (render).
function knownResources() {
  const seen = new Map();
  for (const it of allInstruments(model())) for (const x of it.members || [it]) if (!seen.has(x.resource)) seen.set(x.resource, !!x.online);
  return [...seen].map(([resource, online]) => ({ resource, online }));
}
let knownSent = null;
function sendKnown() {
  const now = knownResources();
  const key = JSON.stringify(now);
  if (knownSent !== null && knownSent === key) return;
  knownSent = key;
  window.__TAURI__.event.emit("aliases-resources", now).catch(() => {});
}
function openAliases() {
  const now = knownResources();
  knownSent = JSON.stringify(now);
  invoke("open_aliases", { resources: now, title: t("alias.all") }).catch(e => toast(String(e), true));
}
function openPictures() {
  invoke("open_pictures", { instruments: allInstruments(model()).map(it => ({ name: titleOf(it.resource) || (it.members ? lanShort(it.resource) : it.resource), texts: pictureTexts(it) })), title: t("settings.pictures") })
    .catch(e => toast(String(e), true));
}
function openTrace() { invoke("open_trace", { title: t("trace.title") }).catch(e => toast(String(e), true)); }
// The bundle is written to the Downloads folder; the service's page says where, with a button that shows it there.
let bundlePath = null;
function makeBundle() {
  act("diag", async () => { bundlePath = await invoke("diag_bundle"); return bundlePath; }, path => t("diag.saved", { path }));
}
function showBundle() { invoke("show_in_folder", { path: bundlePath }).catch(e => toast(String(e), true)); }
function toolsMenu(button) {
  const r = button.getBoundingClientRect();
  openMenu([
    [t("alias.manage"), openAliases],
    [t("picture.manage"), openPictures],
    features.trace ? [t("trace.open"), openTrace] : null,
    "-",
    features.sharing ? [t("group.share"), () => select({ kind: "share" })] : null,
    features.service ? [t("service.menu"), () => select({ kind: "service" })] : null,
    "-",
    [t("about.menu"), () => select({ kind: "about" })],
  ].filter(Boolean).filter((x, i, a) => x !== "-" || a[i - 1] !== "-"), r.left, r.bottom + 4, button, t("action.tools"));
}

// The log's box follows the window's height; a log scrolled to its end stays at its end when the box changes size.
const logResize = new ResizeObserver(es => { for (const e of es) if (e.target.dataset.atEnd !== "no") e.target.scrollTop = e.target.scrollHeight; });

// What a user looks at, and sends, when something is wrong: the end of the daemon's log, and a bundle of everything
// (diag.rs) written to the Downloads folder.
function diagBox() {
  const log = need("diag:log", "log_tail", { lines: 40 });
  const text = log.value ? log.value[0] : "";
  const pre = el("pre", { class: "log mono diag-log", tabindex: "0", "data-scroll": "bottom" }, log.loading ? t("state.loading") : log.error ? log.error : text || t("diag.log_empty"));
  pre.addEventListener("scroll", () => { pre.dataset.atEnd = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 2 ? "" : "no"; });
  logResize.disconnect();
  logResize.observe(pre);
  const path = log.value ? log.value[1] : "";
  const b = tip(box(t("diag.title"),
    el("div", { class: "bar-end" },
      el("button", { type: "button", class: "iconbtn", title: t("action.check_again"), "aria-label": t("action.check_again"), disabled: log.loading, onclick: () => { extra.delete("diag:log"); render(); } }, icon(REFRESH)),
      el("button", { type: "button", disabled: busy.has("diag"), onclick: makeBundle }, t("diag.bundle"))),
    busy.has("diag") ? el("div", { class: "notice" }, t("diag.making")) : null,
    !busy.has("diag") && notes.has("diag") ? el("div", { class: "notice" + (notes.get("diag").error ? " error" : "") },
      el("span", {}, notes.get("diag").text),
      bundlePath && !notes.get("diag").error ? el("button", { type: "button", onclick: showBundle }, t("diag.show")) : null) : null,
    pre,
    // At the box's foot, where Settings has the configuration folder.
    path ? [el("div", { class: "divider" }), pathLine(t("diag.log"), path)] : null), t(features.sharing ? "diag.hint" : "diag.hint_local"));
  b.classList.add("fill");
  return b;
}

// A switch with its label and, under it, one line of what it does.
function switchRow(cb, label, hint) {
  return el("label", { class: "switch" }, cb,
    el("span", { class: "switch-text" }, el("span", {}, label), hint ? el("span", { class: "hint" }, hint) : null));
}

function serviceBox() {
  const running = need("service", "service_running");
  return tip(box(t("service.state"),
    el("div", { class: "bar-end" },
      el("button", { type: "button", class: "iconbtn", title: t("action.check_again"), "aria-label": t("action.check_again"), disabled: running.loading, onclick: () => { extra.delete("service"); render(); } }, icon(REFRESH)),
      serviceStopped
        ? el("button", { type: "button", class: "primary", disabled: busy.has("service"), onclick: () => act("service", startService, null) }, t("action.start_service"))
        : el("button", { type: "button", disabled: busy.has("service") || running.value !== true, onclick: () => act("service", stopService, () => t("service.stopped")) }, t("action.stop_service"))),
    el("div", { class: "row" },
      el("span", { class: "dot " + (running.value ? "ok" : "idle") }),
      el("span", { class: "grow" }, running.loading ? t("state.loading") : running.error ? running.error
        : t(running.value ? "service.now_running" : serviceStopped ? "service.now_stopped" : "service.now_idle"))),
    noteEl("service")), t(features.sharing ? "service.hint" : "service.hint_local"));
}

// Settings: whether a service that stays running lets go of the hardware when idle (daemon.conf). Only sharing or the
// registry keeps a service running, so the row goes where the sharing page is hidden (features.conf).
function releaseRow() {
  if (!features.sharing) return [];
  const rel = need("service:release", "release_idle");
  const relBox = el("input", { type: "checkbox", checked: rel.value === true, disabled: busy.has("service:release") || rel.value === undefined });
  relBox.addEventListener("change", () => act("service:release", async () => {
    await invoke("set_release_idle", { on: relBox.checked });
    extra.delete("service:release");
    return relBox.checked;
  }, on => t(on ? "service.release_on" : "service.release_off")));
  return [
    el("div", { class: "row nowrap" }, switchRow(relBox, t("service.release"), t("service.release_short")), helpMark(t("service.release_hint"))),
    noteEl("service:release"),
  ];
}

// The service's page (from the Tools menu, or the status bar): whether it runs, and the diagnostics. Its explanations
// leave the group out where the sharing page is hidden (features.conf).
function servicePane() {
  const running = need("service", "service_running");
  const pill = running.loading || running.error ? null : running.value ? ["ok", t("service.pill_running")] : ["idle", t(serviceStopped ? "service.pill_stopped" : "service.pill_idle")];
  return [head(null, t("service.menu"), pill), serviceBox(), diagBox()];
}

// The three ways a program finds this VISA without being given its path (Windows only).
function registrationRows() {
  const regs = need("regs", "registrations");
  const detail = st => st === "registered" ? null : /^version /.test(st) ? t("reg.version", { v: st.slice(8) }) : st;
  const rows = (regs.value || []).map(r => {
    const key = "reg:" + r.id;
    const more = r.state ? detail(r.state) : null;
    return el("div", { class: "listrow static" },
      el("span", { class: "dot " + (r.state ? "ok" : "idle") }),
      el("span", { class: "label" },
        el("span", {}, t("reg." + r.id), el("span", { class: "muted" }, " · " + t(r.state ? "reg.on" : "reg.off"))),
        el("span", { class: "sub" }, t("reg." + r.id + ".what")),
        more ? el("span", { class: "sub mono" }, more) : null),
      busy.has(key) ? el("span", { class: "muted" }, t("reg.waiting")) : noteEl(key),
      el("button", { type: "button", disabled: busy.has(key), onclick: () => act(key, async () => {
        await invoke("set_registration", { id: r.id, on: !r.state });
        extra.delete("regs");
        return !r.state;
      }, on => t(on ? "reg.done_on" : "reg.done_off")) }, t(r.state ? "action.unregister" : "action.register")));
  });
  return [
    el("div", { class: "subhead" }, el("span", {}, t("reg.title")), helpMark(t("reg.warning"))),
    el("div", { class: "hint" }, t("reg.warning_short")),
    loading(regs) || el("div", { class: "rows" }, rows),
  ];
}

function zoomSelect() {
  const sel = el("select", { id: "set-zoom", onchange: e => setZoom(Number(e.target.value), false) });
  const list = ZOOMS.includes(zoom) ? ZOOMS : [...ZOOMS, zoom].sort((a, b) => a - b);
  for (const z of list) sel.append(new Option(z + "%", String(z), false, z === zoom));
  return sel;
}

// Settings: whether the absent instruments are in every program's resource list.
function findAllRow() {
  const cb = el("input", { type: "checkbox", checked: listAll, disabled: busy.has("find:all") });
  cb.addEventListener("change", () => act("find:all", async () => {
    await invoke("set_list_all", { on: cb.checked });
    listAll = cb.checked;
    return cb.checked;
  }, on => t(on ? "settings.find_all_on" : "settings.find_all_off")));
  return [
    el("div", { class: "row nowrap" }, switchRow(cb, t("settings.find_all"), t("settings.find_all_short")), helpMark(t("settings.find_all_hint"))),
    noteEl("find:all"),
  ];
}

// Settings: do the console windows close with this one? They stay by default.
function consolesRow() {
  const cb = el("input", { type: "checkbox", checked: stored("console.closeWithMain", "") === "1" });
  cb.addEventListener("change", () => {
    store("console.closeWithMain", cb.checked ? "1" : "");
    invoke("set_close_consoles", { on: cb.checked }).catch(e => toast(String(e), true));
  });
  return switchRow(cb, t("settings.consoles_close"), t("settings.consoles_close_short"));
}

// The marks before Settings' first three labels: a globe, a moon, a magnifier with a plus.
const LANGUAGE_ICON = ["circle cx=12 cy=12 r=10", "M2 12h20", "M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"];
const THEME_ICON = ["M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9z"];
const ZOOM_ICON = ["circle cx=11 cy=11 r=8", "M21 21l-4.3-4.3", "M11 8v6", "M8 11h6"];

// Settings are what the user prefers: this window's looks and ways, and what VISA does for every program here.
// The service, the diagnostics and the windows of their own are elsewhere (the Service page, the Tools menu).
function settingsPane() {
  const lang = el("select", { id: "set-lang", onchange: e => setLanguage(e.target.value) });
  for (const [code, name] of languageNames) lang.append(new Option(name, code, false, code === document.documentElement.lang));
  const theme = el("select", { id: "set-theme", onchange: e => setTheme(e.target.value) });
  const current = document.documentElement.dataset.theme || "system";
  for (const x of ["system", "light", "dark"]) theme.append(new Option(t("theme." + x), x, false, x === current));
  const form = (...rows) => el("div", { class: "form" }, rows.flatMap(([id, mark, label, control]) => [el("label", { for: id, class: "k with-icon" }, icon(mark, 16), label), control]));
  return [
    head(null, t("action.settings"), null),
    box(t("settings.window"), null,
      form(["set-lang", LANGUAGE_ICON, t("setting.language"), lang], ["set-theme", THEME_ICON, t("setting.theme"), theme],
        ["set-zoom", ZOOM_ICON, t("setting.zoom"), el("div", { class: "row nowrap" }, zoomSelect(), helpMark(t("setting.zoom_hint")))]),
      el("div", { class: "divider" }),
      consolesRow()),
    box(t("settings.visa"), null,
      el("div", { class: "hint" }, t("settings.visa_hint")),
      ...findAllRow(),
      ...releaseRow(),
      ...(about && about.os === "windows" ? [el("div", { class: "divider" }), ...registrationRows()] : []),
      // Where every program's VISA settings live (the version and the platform are the About page's).
      about && about.config_dir ? [el("div", { class: "divider" }), pathLine(t("field.config_dir"), about.config_dir)] : null),
  ];
}

// The page the window opens on: the product's name and version, while the tree is being filled (its own spinner says so)
// and after, until the user picks something in the tree; when nothing was found (or the service did not answer), it
// says so instead.
function startPane(m) {
  if (snap === null || (!snap.error && (m.boards.length || allInstruments(m).length))) return [splash()];
  const again = el("button", { type: "button", class: "primary", onclick: refresh }, icon(REFRESH, 16), t("action.refresh"));
  if (snap.error) return [el("div", { class: "start" }, el("h1", {}, t("start.error")), el("div", { class: "error" }, snap.error), again)];
  return [el("div", { class: "start" },
    el("h1", {}, t("start.none")),
    el("div", { class: "muted" }, t("start.none_hint")),
    el("div", { class: "row" }, again,
      el("button", { type: "button", onclick: () => select({ kind: "group", id: "lan" }) }, t("start.add_lan"))))];
}

// The splash is one node kept between renders (by language), so its drawing-in plays once.
let splashNode = null;
function splash() {
  if (splashNode && splashNode.dataset.lang === document.documentElement.lang) return splashNode;
  const logo = el("div", { class: "splash-logo", "aria-hidden": "true" });
  logo.innerHTML = '<svg viewBox="0 0 96 96" width="96" height="96"><defs><linearGradient id="splash-tile" x1="0" y1="0" x2="1" y2="1">' +
    '<stop offset="0" stop-color="var(--logo-from)"/><stop offset="1" stop-color="var(--logo-to)"/></linearGradient></defs>' +
    '<rect width="96" height="96" rx="22" fill="url(#splash-tile)"/>' +
    '<path class="trace" pathLength="1" d="M18 48h13l8.5-24 17 48 8.5-24h13" fill="none" stroke="#FFFFFF" stroke-width="8" stroke-linecap="round" stroke-linejoin="round"/></svg>';
  const OS = { windows: "Windows", linux: "Linux", macos: "macOS" }, ARCH = { x86_64: "x64", aarch64: "ARM64", x86: "x86" };
  const platform = about ? [OS[about.os] || about.os, ARCH[about.arch] || about.arch].filter(Boolean).join(" ") : "";
  splashNode = el("div", { class: "splash", "data-lang": document.documentElement.lang },
    el("div", { class: "splash-grid", "aria-hidden": "true" }),
    logo,
    el("div", { class: "splash-kicker" }, "Viper488"),
    el("h1", {}, "Symbiotic VISA"),
    el("div", { class: "splash-tagline" }, t("splash.tagline")),
    el("div", { class: "splash-chips", "aria-label": t("splash.interfaces") },
      ...["GPIB", "USBTMC", "VXI-11", "HiSLIP", "SOCKET", "ASRL"].map(x => el("span", { class: "mono" }, x))),
    el("div", { class: "splash-meta" },
      about ? el("span", {}, t("splash.version", { v: about.version })) : null,
      platform ? el("span", {}, platform) : null,
      el("span", {}, "VPP-4.3")));
  return splashNode;
}

// A text field whose unsaved text outlives a re-render (drafts, by key); the page is rebuilt whenever
// an answer arrives, and must not take what the user is typing.
const drafts = new Map();
function field(key, initial, attrs) {
  const e = el("input", { type: "text", ...attrs, "data-key": key });
  e.value = drafts.has(key) ? drafts.get(key) : initial;
  e.addEventListener("input", () => drafts.set(key, e.value));
  return e;
}

function renderDetail(m) {
  const d = document.getElementById("detail");
  const f = document.activeElement;
  const focus = f && d.contains(f) && f.dataset.key ? { key: f.dataset.key, from: f.selectionStart, to: f.selectionEnd } : null;
  const top = d.scrollTop;
  let parts;
  if (selected.kind === "instrument") parts = instrumentPane(m, selected.id);
  else if (selected.kind === "board") parts = boardPane(m, selected.id);
  else if (selected.kind === "group") parts = groupPane(m, selected.id);
  else if (selected.kind === "gateway") parts = gatewayPane(m, selected.id);
  else if (selected.kind === "share") parts = sharePane();
  else if (selected.kind === "shared") parts = sharedPane(selected.id);
  else if (selected.kind === "service") parts = servicePane();
  else if (selected.kind === "settings") parts = settingsPane();
  else if (selected.kind === "about") parts = [splash()];
  else parts = startPane(m);
  // A pane leaves out what the platform lacks as null (the registrations off Windows); replaceChildren
  // would show it as the text "null".
  // The splash already there is left alone: put back, it would draw itself in again.
  if (parts.length === 1 && parts[0] === splashNode && d.childNodes.length === 1 && d.firstChild === splashNode) return;
  rendering = true;
  try { d.replaceChildren(...parts.filter(Boolean)); } finally { rendering = false; }
  d.scrollTop = top;
  for (const x of d.querySelectorAll("[data-scroll=bottom]")) x.scrollTop = x.scrollHeight;
  if (focus) {
    const again = d.querySelector('[data-key="' + CSS.escape(focus.key) + '"]');
    if (again) { again.focus({ preventScroll: true }); try { again.setSelectionRange(focus.from, focus.to); } catch (e) { /* not a text field */ } }
  }
}

function renderStatus(m) {
  document.getElementById("refresh").disabled = serviceStopped;
  const service = document.getElementById("service");
  const counts = document.getElementById("counts");
  // The version (features.conf may hide it), in the language now in use.
  document.getElementById("version").textContent = features.version && about ? t("splash.version", { v: about.version }) : "";
  service.replaceChildren();
  service.disabled = !features.service;
  service.title = features.service ? t("status.open_service") : "";
  counts.textContent = "";
  if (snap === null) { service.textContent = t("tree.loading"); return; }
  if (snap.stopped) { service.append(el("span", { class: "dot idle" }), t("service.stopped_status")); return; }
  const ok = !snap.error && snap.boards && !snap.boards.Err;
  service.append(el("span", { class: "dot " + (ok ? "ok" : "warn") }), t(ok ? "service.running" : "service.down"));
  if (ok && gpibPaused) service.append(el("span", { class: "paused", title: t("live.paused_hint") }, " · " + t("live.paused")));
  if (!snap.error) {
    counts.textContent = t("count.adapters", { n: m.boards.length }) + " · " +
      t("count.instruments", { n: allInstruments(m).filter(x => x.online).length });
  }
}

function render() {
  const m = model();
  renderTree(m);
  renderDetail(m);
  if (knownSent !== null) sendKnown();
  renderStatus(m);
}

// ---- live updates ------------------------------------------------------------------------------------

// While the window is shown it follows what it can see change:
//  - adapters, USB instruments and serial ports plugged in or out, every 2 s (`watch`: the system's
//    device lists, nothing sent to an instrument);
//  - every LAN instrument in the tree, configured or discovered, taking a connection or not, every
//    10 s; with discovery on, what newly answers a scan, every 30 s;
//  - instruments behind a LAN/GPIB gateway switched on or off, every 15 s, by asking the gateway which addresses have a listener;
//  - GPIB instruments switched on or off, every 15 s, by probing the boards -- only while no other
//    program is connected to the service (a probe addresses an instrument, which under REN puts it in
//    Remote; each one found is sent back to Local), so a program's instruments are never touched.
// A change brings the tree up to date and the status bar says what came or went. Hidden or minimised,
// nothing is asked, and the service can exit when idle, giving the adapters back.
const SHARED_MS = 10000; // what the group says of the instruments others share: as often as the computers say it
const WATCH_MS = 2000, LAN_MS = 10000, DISCOVER_MS = 30000, GPIB_MS = 15000;
let watchPrint = null, updating = false, gpibPaused = false;

function onlineSet() {
  return new Set(allInstruments(model()).filter(x => x.online).map(x => x.resource));
}

async function liveUpdate(was = null) {
  if (updating) return;
  updating = true;
  try { await liveChange(was); } finally { updating = false; }
}
async function liveChange(was) {
  // `was`: what was online before a change already made to the page's own state (a probe's result).
  const before = was || onlineSet();
  // Names as they are now: what goes loses its *IDN? answer in the reload.
  const names = new Map([...before].map(r => [r, titleOf(r)]));
  for (const k of ["serial", "usb", "lan_check"]) extra.delete(k);
  await reloadSnapshot();
  const after = onlineSet();
  await closeGone();
  const name = r => titleOf(r) || names.get(r) || r;
  const came = [...after].filter(r => !before.has(r)).map(name);
  const went = [...before].filter(r => !after.has(r)).map(name);
  if (came.length || went.length) {
    toast([came.length ? t("live.came", { list: came.join(", ") }) : "", went.length ? t("live.went", { list: went.join(", ") }) : ""].filter(Boolean).join(" "));
  }
}

// A console session to what no longer answers is closed (it would hold a serial port or a USB
// instrument that comes back from other programs), and its console window says so.
async function closeGone() {
  if (!snap || snap.error) return;
  const answering = allInstruments(model()).flatMap(x => x.members ? x.members : [x]).filter(x => x.online).map(x => x.resource);
  try {
    const gone = await invoke("close_sessions_except", { keep: answering });
    // The console windows whose session it was say so in their logs.
    if (gone.length) await window.__TAURI__.event.emit("console-gone", { resources: gone });
  } catch (e) { /* nothing held */ }
}

// The registry's word on the other computers' instruments, read again (only the registry is asked; nobody's instrument is touched).
function sharedTick() {
  if (quiet()) return;
  // The group's pictures: when the registry has a newer set, the copy follows and what is shown is read again.
  invoke("pictures_sync").then(async changed => { if (changed) { pictureImages.clear(); await loadPictures(); render(); } }, () => {});
  const old = extra.get("shared_tree");
  if (!old || !old.value || !old.value.joined) return;
  invoke("share_status").then(async v => {
    extra.set("shared_tree", { value: v });
    // The group's network instruments: an alias another computer added, changed or took away.
    if (await reloadAliases()) { await reachGroupOnly(); reloadSnapshot(); } else render();
  }, () => {});
}

function quiet() { return document.hidden || updating || snap === null || !!snap.error; }

async function watchTick() {
  if (quiet()) return;
  let print;
  try { print = JSON.stringify(await invoke("watch")); } catch (e) { return; }
  const changed = watchPrint !== null && print !== watchPrint;
  watchPrint = print;
  if (changed) liveUpdate();
}

// Every LAN resource the tree has: does it take a connection as the tree says it does?
async function lanTick() {
  if (quiet()) return;
  const m = model();
  const lan = m.lan.flatMap(x => x.members);
  if (!lan.length && !m.gateways.length) return;
  // The instruments behind a gateway are there by their address's listener (gatewayTick), not by the gateway taking a connection.
  const direct = lan.filter(x => !isGatewayDevice(x.resource));
  // A gateway answers when its host takes a connection: asked by the name of its core channel.
  const gwNames = m.gateways.map(g => "TCPIP0::" + g.ip + "::inst0::INSTR");
  let reach;
  try { reach = await invoke("lan_reach", { resources: [...lan.map(x => x.resource), ...gwNames] }); } catch (e) { return; }
  const answers = new Map(reach);
  // An instrument known only by its group alias is there as this check says (model reads groupOnline).
  const only = new Set(groupOnlyNow().map(r => r.toLowerCase()));
  for (const [r, ok] of answers) if (only.has(r.toLowerCase())) groupOnline.set(r.toLowerCase(), ok);
  let changed = direct.some(x => answers.has(x.resource) && answers.get(x.resource) !== x.online);
  m.gateways.forEach((g, i) => {
    const up = answers.get(gwNames[i]);
    if (up !== undefined && up !== g.online) { gwOnline.set(g.ip, up); changed = true; }
  });
  if (changed) liveUpdate();
}

// With discovery on: an instrument that answers a scan and is not in the tree yet.
async function discoverTick() {
  if (quiet()) return;
  let found, scanned;
  try {
    if (!(await invoke("lan")).discover) return;
    scanned = await invoke("lan_scan"); found = scanned.names;
  } catch (e) { return; }
  const known = new Set(model().lan.flatMap(x => x.members).filter(x => x.online).map(x => x.resource));
  if (found.some(r => !known.has(r))) liveUpdate();
  const gws = new Set(model().gateways.map(g => g.ip));
  if (scanned.gateways.some(g => !gws.has(g.ip))) liveUpdate();
}

// GPIB instruments switched on or off: the addresses that answer against the tree's, per board.
async function gpibTick() {
  if (quiet()) return;
  let scan;
  try { scan = await invoke("bus_scan"); } catch (e) { return; }
  if (gpibPaused !== scan.busy) { gpibPaused = scan.busy; renderStatus(model()); }
  if (scan.busy) return;
  const m = model();
  for (const b of scan.boards) {
    if (!b.found) continue; // this board could not be asked: nothing concluded
    const board = m.boards.find(x => x.board === b.board);
    const shown = new Set((board ? board.instruments : []).filter(x => x.online)
      .map(x => Number(x.resource.split("::")[1])));
    const now = new Set(b.found);
    if (shown.size !== now.size || [...now].some(p => !shown.has(p))) { liveUpdate(); return; }
  }
}

// The gateways' addresses that have a listener, probed (gateway_scan) and kept in gwListeners; true if any gateway's set changed.
async function scanGateways() {
  const ips = model().gateways.filter(g => g.online && g.instruments.some(i => gpibPad(i.resource) !== null)).map(g => g.ip);
  if (!ips.length) return false;
  let scans;
  try { scans = await invoke("gateway_scan", { ips }); } catch (e) { return false; }
  let changed = false;
  for (const s of scans) {
    if (!s.found) continue; // not asked, or no conclusion: what the page shows stays
    const now = new Set(s.found), was = gwListeners.get(s.ip);
    if (!was || was.size !== now.size || [...now].some(p => !was.has(p))) changed = true;
    gwListeners.set(s.ip, now);
  }
  return changed;
}

// Instruments behind a gateway switched on or off, as the boards' are: a change brings the tree up to date, and what came is asked *IDN?.
async function gatewayTick() {
  // Not while an *IDN? is under way: a probe addresses the bus in the middle of its write and read.
  if (quiet() || [...idn.values()].some(a => a.pending)) return;
  const before = onlineSet();
  if (await scanGateways()) liveUpdate(before);
}

setInterval(watchTick, WATCH_MS);
setInterval(lanTick, LAN_MS);
setInterval(discoverTick, DISCOVER_MS);
setInterval(gpibTick, GPIB_MS);
setInterval(gatewayTick, GPIB_MS);
setInterval(sharedTick, SHARED_MS);
document.addEventListener("visibilitychange", () => { if (!document.hidden) { watchTick(); lanTick(); } });

// ---- zoom ------------------------------------------------------------------------------------------

// The zoom itself is in common.js; here is what the window says and does when it changes.
hooks.zoomed = () => { if (selected.kind === "settings") render(); };

// A resource name may break after each "::" (where it reads as a break), not in the middle of a word.
function resName(r) {
  const parts = resParts(r);
  return el("span", { class: "resname" }, parts.flatMap((p, i) => i ? ["::", el("wbr"), p] : [p]));
}

// ---- the context menu, the dialog, the browser's own keys --------------------------------------------

// The webview's own menu (Back, Reload, Save as, Print) has no place here, and Reload would drop a
// window's page and its state: it stays only in text fields (cut, copy, paste). An instrument, in the
// tree or a list, has a menu of its own; selected text elsewhere is offered Copy.

let menu = null; // {el, back}: the open menu and what had the focus

function isTextField(x) {
  return x && x.closest && x.closest('input[type="text"], input[type="number"], input:not([type]), textarea');
}


function closeMenu(refocus) {
  if (!menu) return;
  const back = menu.back;
  menu.el.remove();
  menu = null;
  if (refocus && back && document.contains(back)) back.focus();
}

// Items: [label, action, {disabled, title, danger}] or "-" for a separator; `name` says whose menu it is.
function openMenu(items, x, y, back, name) {
  closeMenu();
  const list = el("div", { class: "menu", role: "menu", "aria-label": name || null });
  for (const item of items) {
    if (item === "-") { list.append(el("div", { class: "sep", role: "separator" })); continue; }
    const [label, run, o = {}] = item;
    list.append(el("button", {
      type: "button", role: "menuitem", class: o.danger ? "danger" : null, disabled: !!o.disabled, title: o.title || null,
      onclick: () => { closeMenu(); run(); },
    }, label));
  }
  list.addEventListener("keydown", e => {
    const items = [...list.querySelectorAll("button:not(:disabled)")];
    const i = items.indexOf(document.activeElement);
    if (e.key === "Escape" || e.key === "Tab") { e.preventDefault(); closeMenu(true); return; }
    // A menu whose items are all disabled has nothing to move to.
    if (!items.length) return;
    if (e.key === "ArrowDown") { e.preventDefault(); items[(i + 1) % items.length].focus(); }
    else if (e.key === "ArrowUp") { e.preventDefault(); items[(i - 1 + items.length) % items.length].focus(); }
    else if (e.key === "Home") { e.preventDefault(); items[0].focus(); }
    else if (e.key === "End") { e.preventDefault(); items[items.length - 1].focus(); }
  });
  document.body.append(list);
  // Inside the window, whichever way it had to open.
  const r = list.getBoundingClientRect();
  list.style.left = Math.max(4, Math.min(x, window.innerWidth - r.width - 4)) + "px";
  list.style.top = Math.max(4, Math.min(y, window.innerHeight - r.height - 4)) + "px";
  menu = { el: list, back };
  const first = list.querySelector("button:not(:disabled)");
  if (first) first.focus();
}

// Open an instrument's page, then put the focus in one of its fields (a data-key).
function openAndFocus(r, key) {
  select({ kind: "instrument", id: r });
  const f = document.querySelector('#detail [data-key="' + CSS.escape(key) + '"]');
  if (f) { f.scrollIntoView({ block: "center" }); f.focus(); f.select(); }
}

function instrumentMenu(r) {
  const m = model();
  const it = itemOf(m, r) || { resource: r, online: false, hidden: false, configured: false };
  const s = settingsOf(r);
  const a = idn.get(r);
  return [
    [t("menu.open_console"), () => openConsole(r)],
    [t("menu.identify"), () => { select({ kind: "instrument", id: r }); identify(r); }, { disabled: !it.online || (a && a.pending) }],
    features.display_name ? [t("menu.rename"), () => openAndFocus(r, "name:" + r)] : null,
    "-",
    [t(s.hidden ? "menu.show" : "menu.hide"), async () => {
      const err = await saveSettings(r, { ...s, hidden: !s.hidden });
      if (err) toast(err, true);
      else toast(t(s.hidden ? "toast.shown" : "toast.hidden", { name: titleOf(r) || r }));
      render();
    }],
    [t("menu.remove_dialog"), () => removeInstrument(r), removable([it]).n ? { danger: true } : { disabled: true, title: t("menu.remove_instrument_disabled") }],
  ].filter(Boolean);
}

// What removing network instruments takes away: their lines in lan.conf, and the group's aliases for them (an
// instrument known by its group alias alone is in the tree because of it). Nothing else of a network instrument can be
// removed: one the network scan finds is there while discovery is on.
function removable(items) {
  const resources = items.flatMap(it => it.members ? it.members : [it]);
  const conf = resources.filter(x => x.configured).map(x => x.resource);
  const names = new Set(resources.map(x => x.resource.toLowerCase()));
  const aliases = aliasList.filter(a => a.group && names.has(a.resource.toLowerCase()));
  return { conf, aliases, resources: resources.map(x => x.resource), n: conf.length + aliases.length };
}
// The confirmation's text for that: the lan.conf lines, then the group's aliases, which go from every computer of the group.
function removalText(text, what) {
  return [
    what.conf.length ? text : null,
    what.conf.length ? el("ul", { class: "mono" }, what.conf.map(x => el("li", {}, x))) : null,
    what.aliases.length ? t("dialog.remove_group_aliases", { n: what.aliases.length }) : null,
    what.aliases.length ? el("ul", { class: "mono" }, what.aliases.map(a => el("li", {}, a.alias + " = " + a.resource))) : null,
    // Its settings and what was recorded of it go too.
    what.resources.some(x => settings.has(x)) ? t("dialog.remove_settings") : null,
  ].filter(Boolean);
}
async function removeAll(what) {
  for (const x of what.conf) await invoke("lan_remove", { resource: x });
  for (const a of what.aliases) await invoke("set_alias", { alias: a.alias, resource: null });
  // A removed instrument's settings and what was recorded of it go too: nothing is left for the next one at its address.
  if (what.resources.length) await invoke("forget_instrument", { resources: what.resources });
  settings = settingsMap(await invoke("instrument_settings"));
  extra.delete("lan_check");
  if (what.aliases.length) await reloadAliases();
}

async function removeInstrument(r) {
  const it = itemOf(model(), r);
  if (!it) return;
  const what = removable([it]);
  if (!what.n) return;
  const name = titleOf(r) || r;
  if (!await confirmDialog(t("dialog.remove_instrument_title"), removalText(t("dialog.remove_instrument_text", { name, n: what.conf.length }), what), t("menu.remove"), true)) return;
  const gone = (it.members || [it]).map(x => x.resource);
  try {
    await removeAll(what);
    if (selected.kind === "instrument" && selected.id === r) select({ kind: "group", id: "lan" });
    await reloadSnapshot();
    const still = allInstruments(model()).some(x => (x.members || [x]).some(y => gone.includes(y.resource)));
    toast(t(still ? "toast.instrument_removed_found" : "toast.instrument_removed", { name }));
  } catch (e) { toast(String(e), true); }
}

// A GPIB board's menu: Remove, for an adapter that is not attached (the daemon forgets its number, boards.rs).
function boardMenu(id) {
  const b = model().boards.find(x => x.board === id);
  const why = !b ? null : !b.present ? null : t(b.kind === "prologix" ? "menu.remove_bridge_attached" : "menu.remove_board_attached");
  return [[t("menu.remove"), () => removeBoard(id), why ? { disabled: true, title: why } : { danger: true }]];
}

async function removeBoard(id) {
  try {
    await invoke("forget_board", { board: Number(id.slice(4)) });
    if (selected.kind === "board" && selected.id === id) select({ kind: "start" });
    await reloadSnapshot();
    toast(t("toast.board_removed", { board: id }));
  } catch (e) { toast(String(e), true); }
}

// What a gateway's Remove takes out: what removing each of its instruments would.
function gatewayRemovable(ip) {
  const gw = model().gateways.find(x => x.ip === ip);
  return removable(gw ? gw.instruments : []);
}

// A LAN/GPIB gateway's menu: Remove takes its instruments' lan.conf lines and group aliases; one discovery found has nothing to take.
function gatewayMenu(ip) {
  return [[t("menu.remove_dialog"), () => removeGateway(ip), gatewayRemovable(ip).n ? { danger: true } : { disabled: true, title: t("menu.remove_gateway_disabled") }]];
}

async function removeGateway(ip) {
  const what = gatewayRemovable(ip);
  if (!what.n) return;
  if (!await confirmDialog(t("dialog.remove_gateway_title"), removalText(t("dialog.remove_gateway_text", { ip, n: what.conf.length }), what), t("menu.remove"), true)) return;
  try {
    await removeAll(what);
    await reloadSnapshot();
    const still = model().gateways.some(g => g.ip === ip);
    if (!still && selected.kind === "gateway" && selected.id === ip) select({ kind: "group", id: "lan" });
    toast(t(still ? "toast.gateway_removed_found" : "toast.gateway_removed", { ip }));
  } catch (e) { toast(String(e), true); }
}

// The menu for a row of the tree, by what it is; null for a row that has none.
function rowMenu(target) {
  const row = target && target.closest && target.closest("[data-resource], [data-board], [data-gateway]");
  if (!row) return null;
  if (row.dataset.resource) return { row, items: instrumentMenu(row.dataset.resource), name: row.dataset.resource };
  if (row.dataset.board) return { row, items: boardMenu(row.dataset.board), name: row.dataset.board };
  return { row, items: gatewayMenu(row.dataset.gateway), name: row.dataset.gateway };
}

document.addEventListener("contextmenu", e => {
  if (isTextField(e.target)) return; // the webview's own: cut, copy, paste
  e.preventDefault();
  // Windows sends the menu key's own contextmenu on its release, to the focus, which is by then in
  // our menu: that one is not a request to close it.
  if (menu && menu.el.contains(e.target)) return;
  const m = rowMenu(e.target);
  if (m) { openMenu(m.items, e.clientX, e.clientY, m.row, t("menu.of", { name: m.name })); return; }
  const text = String(window.getSelection() || "");
  if (text) openMenu([[t("menu.copy"), () => copyText(text)]], e.clientX, e.clientY, document.activeElement);
  else closeMenu();
});
// The Tools button closes its own menu on its click (a toggle), not here.
document.addEventListener("mousedown", e => { if (menu && !menu.el.contains(e.target) && !e.target.closest("#open-tools")) closeMenu(); });
window.addEventListener("blur", () => closeMenu());
window.addEventListener("resize", () => closeMenu());
// Scrolled by the user (not by a pane being rebuilt, which keeps its place): the menu would float away.
document.addEventListener("wheel", e => { if (menu && !menu.el.contains(e.target)) closeMenu(); }, { capture: true, passive: true });
// The mouse's back and forward buttons would navigate away from the page.
window.addEventListener("mouseup", e => { if (e.button === 3 || e.button === 4) e.preventDefault(); });

// The browser's keys: reload is the window's own Refresh; back, forward, print, save, find and view
// source do nothing. The menu key (or Shift+F10) opens a tree row's menu from the keyboard.
document.addEventListener("keydown", e => {
  const k = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  const mod = e.ctrlKey || e.metaKey;
  if (k === "F5" || k === "BrowserRefresh" || (mod && k === "r")) { e.preventDefault(); if (!e.repeat) refresh(); return; }
  // Zoom: = and + (with or without Shift, or the keypad's), - and _, 0 (the keypad's too).
  if (mod && !e.altKey && ["=", "+", "-", "_", "0"].includes(k)) {
    e.preventDefault();
    if (k === "0") setZoom(100, true); else zoomStep(k === "-" || k === "_" ? -1 : 1);
    return;
  }
  if (k === "BrowserBack" || k === "BrowserForward" || (e.altKey && (k === "ArrowLeft" || k === "ArrowRight")) ||
      (mod && ["p", "s", "f", "g", "u", "o", "j", "h"].includes(k)) || k === "F3" || k === "F7") {
    e.preventDefault();
    return;
  }
  if (k === "ContextMenu" || (e.shiftKey && k === "F10")) {
    const m = rowMenu(document.activeElement);
    if (m) {
      e.preventDefault();
      const r = m.row.getBoundingClientRect();
      openMenu(m.items, r.left + 24, r.bottom - 4, m.row, t("menu.of", { name: m.name }));
    }
  }
}, true);

// ---- start -------------------------------------------------------------------------------------

// The tree's width: dragged on the line between the two sides, or the arrow keys on it; remembered.
function treeWidth(px, keep) {
  const w = Math.round(Math.min(720, Math.max(200, px)));
  document.documentElement.style.setProperty("--tree-w", w + "px");
  if (keep) store("tree.width", String(w));
  return w;
}
function splitter() {
  const bar = document.getElementById("splitter");
  const side = document.getElementById("tree");
  let width = treeWidth(Number(stored("tree.width", "320")) || 320, false);
  bar.addEventListener("pointerdown", e => {
    e.preventDefault();
    bar.setPointerCapture(e.pointerId);
    bar.classList.add("dragging");
    const left = side.getBoundingClientRect().left;
    const move = ev => { width = treeWidth(ev.clientX - left, false); };
    const up = () => {
      bar.classList.remove("dragging");
      bar.removeEventListener("pointermove", move);
      bar.removeEventListener("pointerup", up);
      width = treeWidth(side.getBoundingClientRect().width, true);
    };
    bar.addEventListener("pointermove", move);
    bar.addEventListener("pointerup", up);
  });
  bar.addEventListener("keydown", e => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    width = treeWidth(side.getBoundingClientRect().width + (e.key === "ArrowLeft" ? -16 : 16), true);
  });
}

let languageNames = [];

async function main() {
  for (const code of LANGUAGES) {
    const names = await (await fetch("locales/" + code + ".json")).json();
    languageNames.push([code, names["language.name"] || code]);
  }
  const start = await invoke("start_options");
  const guess = (navigator.language || "en").toLowerCase().startsWith("zh") ? "zh-CN" : "en";
  // --lang and --theme are for this run only: remembered, they would stand in for what the user chose in Settings
  // (the theme follows the system until the user picks one).
  const asked = LANGUAGES.includes(start.lang);
  const lang = asked ? start.lang : stored("language", guess);
  if (start.theme) setTheme(start.theme, false); else setTheme(stored("theme", "system"));
  if (zoom !== 100) setZoom(zoom, false);

  splitter();
  document.getElementById("refresh").addEventListener("click", refresh);
  document.getElementById("open-settings").addEventListener("click", () => select({ kind: "settings" }));
  document.getElementById("open-tools").addEventListener("click", e => { if (menu) closeMenu(); else toolsMenu(e.currentTarget); });
  document.getElementById("service").addEventListener("click", () => select({ kind: "service" }));
  features = await invoke("features").catch(() => features);
  about = await invoke("about");
  listAll = await invoke("list_all").catch(() => false);
  await loadPictures();
  invoke("set_close_consoles", { on: stored("console.closeWithMain", "") === "1" }).catch(() => {});
  await setLanguage(lang, !asked);
  // On a computer in a group a network instrument's alias is the group's: one still kept here is moved there.
  try {
    const moved = await invoke("aliases_move");
    if (moved.length) toast(t("alias.moved", { list: moved.join("; ") }));
  } catch (e) { toast(String(e), true); }
  await refresh();
  await migrateHidden();
  if (start.page) {
    const [kind, ...rest] = start.page.split(":");
    // A page of a feature this window does not show is not opened by --page either.
    const shown = (kind !== "share" && kind !== "shared" && !(kind === "group" && rest[0] === "shared") || features.sharing) && (kind !== "service" || features.service);
    if (shown) select(kind === "settings" || kind === "share" || kind === "service" || kind === "about" ? { kind } : { kind, id: rest.join(":") });
  }
}
main().catch(e => {
  // Whatever stopped the start (a missing locale file, a command that failed) is shown where the
  // tree would be, not left as a blank window.
  const tree = document.getElementById("tree");
  tree.replaceChildren(el("div", { class: "empty error" }, "The window could not start: " + String(e)));
});

// A picture was saved or removed in that window: the identifications show the new ones.
window.__TAURI__.event.listen("pictures-changed", async () => {
  pictureImages.clear();
  await loadPictures();
  render();
});
// The aliases window added, removed or bound an alias, or changed an alias or changed how a resource name is listed.
window.__TAURI__.event.listen("aliases-changed", async () => {
  [aliasList, identityList] = await Promise.all([invoke("aliases"), invoke("identities")]);
  settings = settingsMap(await invoke("instrument_settings"));
  render();
});
