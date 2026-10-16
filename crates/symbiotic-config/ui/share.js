// The sharing page: this computer's place in a group of computers that share aliases. Uses what app.js has:
// need/act/busy/notes, field/drafts, box/head/tip, the tree's select, and the commands in src/share.rs. The registry is
// asked when the page is shown and when it is refreshed, not on a timer.

function dotFor(online) { return "dot " + (online === true ? "ok" : online === false ? "idle" : "warn"); }

function shareStatusBox(s) {
  const refresh = el("button", { type: "button", class: "iconbtn", title: t("action.check_again"), "aria-label": t("action.check_again"),
    onclick: () => { extra.delete("share"); render(); } }, icon(REFRESH));
  const on = el("input", { type: "checkbox", checked: s.sharing, disabled: busy.has("share:on") });
  on.addEventListener("change", () => act("share:on", async () => {
    const said = await invoke("share_set", { on: on.checked });
    extra.delete("share");
    return said;
  }, said => (on.checked ? t("share.serving_on") : t("share.serving_off")) + " " + said));
  const fol = el("input", { type: "checkbox", checked: s.follow, disabled: busy.has("share:follow") });
  fol.addEventListener("change", () => act("share:follow", async () => {
    const said = await invoke("share_follow", { on: fol.checked });
    extra.delete("share");
    return said;
  }, said => (fol.checked ? t("share.follow_on") : t("share.follow_off")) + " " + said));
  const follow = el("div", { class: "row nowrap" }, switchRow(fol, t("share.follow"), t("share.follow_hint_short")), helpMark(t("share.follow_hint")));
  return tip(box(t("share.group"), refresh,
    el("div", { class: "rows" },
      el("div", { class: "listrow static" },
        el("span", { class: s.error ? "dot warn" : "dot ok" }),
        el("span", { class: "label" },
          el("span", {}, s.registry),
          el("span", { class: "sub" }, t("share.as", { name: s.name }) + (s.version != null ? " · " + t("share.version", { n: s.version }) : ""))))),
    s.error ? el("div", { class: "warning" }, t("share.registry_down", { error: s.error })) : null,
    switchRow(on, t("share.serve"), t("share.serve_hint", { listen: s.listen })),
    follow,
    noteEl("share:follow"),
    el("div", { class: "hint" }, s.autostart === "on" ? t("share.autostart_on") : s.autostart === "off" ? t("share.autostart_off")
      : t("share.autostart_unsupported", { why: s.autostart_why || "" })),
    noteEl("share:on")), t("share.group_hint"));
}

function shareJoinBox(s) {
  const code = field("share:code", "", { placeholder: "symshare1/…", "aria-label": t("share.code"), class: "grow" });
  const name = field("share:name", s.name, { "aria-label": t("share.name") });
  const go = el("button", { type: "button", class: "primary", disabled: busy.has("share:join"), onclick: () => act("share:join", async () => {
    const addr = await invoke("share_join", { code: drafts.get("share:code") || "", name: drafts.has("share:name") ? drafts.get("share:name") : s.name });
    drafts.delete("share:code");
    extra.delete("share");
    return addr;
  }, addr => t("share.joined", { addr })) }, t("share.join_button"));
  return box(t("share.join"), null,
    el("div", { class: "hint" }, tNodes("share.join_hint", { a: el("code", {}, "symbiotic-registry token"), b: el("code", {}, "symbiotic-ctl share registry token") })),
    el("div", { class: "row" }, el("label", { class: "inline grow" }, el("span", { class: "k" }, t("share.code")), code)),
    el("div", { class: "row" }, el("label", { class: "inline" }, el("span", { class: "k" }, t("share.name")), name), go),
    noteEl("share:join"));
}

function myAliasRow(s, a) {
  const key = "share:alias:" + a.alias;
  const cb = el("input", { type: "checkbox", checked: a.shared, disabled: busy.has(key) });
  cb.addEventListener("change", () => act(key, async () => {
    await invoke("share_alias", { alias: a.alias, on: cb.checked });
    extra.delete("share");
  }, null));
  let state = null;
  if (a.shared) {
    if (a.problem) state = el("span", { class: "error" }, t("share.problem_" + a.problem));
    else if (!s.sharing) state = el("span", { class: "muted" }, t("share.waiting_off"));
    else if (a.claimed) state = el("span", { class: "pill ok" }, t("share.is_shared"));
    else state = el("span", { class: "muted" }, t("share.not_yet"));
  }
  return el("div", { class: "listrow static" },
    el("label", { class: "switch" }, cb, el("span", { class: "label" }, el("span", {}, a.alias), el("span", { class: "sub mono" }, resName(a.resource)),
      features.alias_binding && unitOf(a.alias) ? el("span", { class: "sub" }, t("unit.bound", { vendor: unitOf(a.alias).vendor, model: unitOf(a.alias).model, serial: unitOf(a.alias).serial })) : null)),
    state, noteEl(key));
}

function shareMineBox(s) {
  return tip(box(t("share.mine"), null,
    s.mine.length ? el("div", { class: "rows" }, s.mine.map(a => myAliasRow(s, a))) : el("div", { class: "hint" }, t("share.mine_none"))),
    t("share.mine_hint"));
}

// Who used this computer's shared instruments (usage.log beside the daemon's log), newest first.
function shareUsedBox(s) {
  const pad = n => String(n).padStart(2, "0");
  const when = sec => {
    const d = new Date(sec * 1000);
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  };
  const rows = s.used.map(u => el("div", { class: "listrow static" },
    el("span", { class: "label" },
      el("span", {}, `${u.alias} · ${u.member}`),
      el("span", { class: "sub" }, when(u.began) + " · " + t("share.used_row", { secs: u.secs.toFixed(1), n: u.requests })
        + (u.refused ? " · " + t("share.used_refused", { n: u.refused }) : "")),
      el("span", { class: "sub mono" }, `${u.key} · ${u.from}`))));
  // The newest few; the rest folded under them, so the record does not push the group's panels down the page.
  const FEW = 3;
  return tip(box(t("share.used"), null,
    rows.length ? el("div", { class: "rows" }, rows.slice(0, FEW)) : el("div", { class: "hint" }, t("share.used_none")),
    rows.length > FEW ? el("details", { class: "more" }, el("summary", {}, t("share.used_more", { n: rows.length - FEW })),
      el("div", { class: "rows" }, rows.slice(FEW))) : null), t("share.used_hint"));
}

function shareOthersBox(s) {
  const others = s.others.filter(x => !x.mine);
  // A network instrument's alias is the group's, not another computer's: it is in the tree with this computer's own.
  const rows = others.filter(x => !x.group).map(x => el("div", { class: "listrow static" },
    el("span", { class: dotFor(x.online) }),
    el("span", { class: "label" },
      el("span", {}, x.alias),
      el("span", { class: "sub" }, t("share.kind_relay", { owner: x.owner })),
      el("span", { class: "sub mono" }, resName(x.resource)))));
  return tip(box(t("share.others"), null,
    rows.length ? el("div", { class: "rows" }, rows) : el("div", { class: "hint" }, t("share.others_none"))), t("share.others_hint"));
}

function shareComputersBox(s) {
  return box(t("share.computers"), null,
    el("div", { class: "rows" }, s.computers.map(c => el("div", { class: "listrow static" },
      el("span", { class: dotFor(c.online) }),
      el("span", { class: "label" },
        el("span", {}, c.name + (c.mine ? " " + t("share.this_one") : "")),
        el("span", { class: "sub mono" }, c.key + (c.addresses.length ? " · " + c.addresses.join(", ") : "")))))));
}

function shareRegistryBox(s) {
  const code = extra.get("share:code");
  const get = el("button", { type: "button", disabled: busy.has("share:token"), onclick: () => act("share:token", async () => {
    const text = (await invoke("share_registry", { line: "token" })).split("\n")[0];
    extra.set("share:code", { value: text });
    return text;
  }, null) }, t("share.registry_token"));
  return tip(box(t("share.registry"), null,
    el("div", { class: "row" }, get, code && code.value ? el("span", { class: "mono selectable" }, code.value) : null, code && code.value ? copyButton(code.value) : null),
    code && code.value ? el("div", { class: "hint" }, t("share.registry_token_hint")) : null,
    noteEl("share:token"),
    el("div", { class: "rows" }, s.computers.filter(c => !c.mine).map(c => {
      const key = "share:revoke:" + c.name;
      return el("div", { class: "listrow static" }, el("span", { class: "label" }, el("span", {}, c.name)),
        el("button", { type: "button", class: "danger-outline", disabled: busy.has(key), onclick: async () => {
          if (!await confirmDialog(t("share.revoke_title", { name: c.name }), t("share.revoke_text"), t("share.revoke"), true)) return;
          act(key, async () => { await invoke("share_registry", { line: "revoke " + c.name }); extra.delete("share"); }, null);
        } }, t("share.revoke")));
    }))), t("share.registry_hint"));
}

function sharePane() {
  const st = need("share", "share_status");
  const s = st.value;
  const parts = [head(t("share.kicker"), t("group.share"), null)];
  if (!s) { parts.push(loading(st)); return parts; }
  if (st.error) parts.push(loading(st));
  if (!s.joined) {
    parts.push(el("div", { class: "hint" }, t("share.intro")), shareJoinBox(s));
  } else {
    parts.push(shareStatusBox(s), shareMineBox(s));
    if (s.sharing || s.used.length) parts.push(shareUsedBox(s));
    parts.push(shareOthersBox(s), shareComputersBox(s));
    if (s.hidden_aliases.length) parts.push(el("div", { class: "warning" }, t("share.hidden_aliases", { names: s.hidden_aliases.join(", ") })));
  }
  if (s.registry_here) parts.push(shareRegistryBox(s));
  return parts;
}

// ---- the instruments other computers share, in the tree ---------------------------------------------------

// What the window does with each (instruments.conf's `shared:` lines), by lower-cased alias: asked *IDN? on its own only when the
// user says so (it is another computer's instrument, and its owner may be using it), hidden or not.
let sharedViews = new Map();
function sharedViewOf(alias) { return sharedViews.get(alias.toLowerCase()) || { idn: false, hidden: false }; }

async function saveSharedView(alias, next) {
  try {
    await invoke("set_shared_settings", { settings: { alias, idn: next.idn, hidden: next.hidden } });
    sharedViews.set(alias.toLowerCase(), next);
    return null;
  } catch (e) { return String(e); }
}

// The ones the user lets the window ask, once each after a refresh, when they are seen to be there. Out of the tree's drawing:
// asking draws the page again.
function askShared(others) {
  for (const x of others) {
    const v = sharedViewOf(x.alias);
    if (v.idn && !v.hidden && sharedPresent(x) === true && !idn.has(x.alias)) {
      idn.set(x.alias, { pending: true });
      setTimeout(() => identify(x.alias, true), 0);
    }
  }
}

// Manufacturer, model and serial from the unit a bound alias names ("vendor | model | serial"), as idnParts gives them; null if unbound.
function identityParts(x) {
  if (!x.identity) return null;
  const [vendor, model, serial] = x.identity.split("|").map(p => p.trim());
  return { vendor: vendor || null, model: model || null, serial: serial || null, firmware: null };
}

// What the registry says the others share, and, in each heartbeat, whether each instrument is there (the same answer as the sharing page,
// asked every ten seconds while the window is shown, see sharedTick).
function sharedByOthers() {
  // Not shown (features.conf): nothing of the group's in the tree, and the registry is not asked.
  if (!features.sharing) return [];
  const st = need("shared_tree", "share_status");
  // A network instrument's alias is the group's: its instrument is in the tree with this computer's own. What is left
  // belongs to another computer and is reached through it.
  const list = st.value && st.value.joined ? st.value.others.filter(x => !x.mine && !x.group) : [];
  // Where the owner does not say, or says it long ago (GPIB with the adapter let go, a serial port): asked of the owner once per refresh.
  const ask = list.filter(x => x.online !== false && !fresh(x)).map(x => x.alias);
  if (ask.length) need("shared_probe", "share_probe", { aliases: ask });
  return list;
}

const SAID_FRESH = 60; // seconds: an owner's word older than this is asked again
function fresh(x) { return x.present !== null && x.present !== undefined && x.age <= SAID_FRESH; }

// true: the instrument answers; false: it does not (or its computer is off); undefined: not known (not asked yet, or a LAN one its owner has not said).
function sharedPresent(x) {
  if (x.online === false) return false;
  if (fresh(x)) return x.present;
  const p = extra.get("shared_probe");
  if (!p || p.loading) return undefined;
  const r = p.value && p.value.find(y => y.alias === x.alias);
  return r ? !r.error : undefined;
}

async function openSharedConsole(x) {
  try { await invoke("open_console", { resource: x.alias, members: [{ resource: x.alias, online: x.online !== false, proto: t("shared.via") }], via: x.alias, title: t("console.window_title", { name: x.alias }) }); }
  catch (e) { toast(String(e), true); }
}

function sharedPane(name) {
  const x = sharedByOthers().find(o => o.alias === name);
  if (!x) return [head(t("shared.kicker"), name, ["idle", t("instrument.gone")])];
  const a = idn.get(x.alias);
  // Not asked (or no answer): what the alias's binding says the unit is, if it is bound.
  const bound = identityParts(x);
  const p = a && a.text ? idnParts(a.text) : bound;
  const down = x.online === false;
  const present = sharedPresent(x);
  const probe = (extra.get("shared_probe") || {}).value;
  const why = present === false && !down && !fresh(x) && probe ? (probe.find(y => y.alias === x.alias) || {}).error : null;
  const pill = down ? ["idle", t("shared.owner_offline", { owner: x.owner })]
    : present === false ? ["idle", t("shared.not_there", { owner: x.owner })]
    : a && a.error ? ["warn", t("instrument.no_answer")]
    : null; // answering: the tree's dot says it; the others say why it is not there
  let answer = bound ? t("shared.idn_from_binding") : t("instrument.idn_not_asked");
  if (a && a.pending) answer = t("instrument.asking");
  else if (a && a.error) answer = t("instrument.idn_error", { error: a.error });
  const v = sharedViewOf(x.alias);
  const ask = el("input", { type: "checkbox", checked: v.idn });
  const hide = el("input", { type: "checkbox", checked: v.hidden });
  const save = async () => {
    const err = await saveSharedView(x.alias, { idn: ask.checked, hidden: hide.checked });
    toast(err || t("settings.saved"), !!err);
    if (!err && ask.checked && !hide.checked && !down && !idn.has(x.alias)) identify(x.alias, true);
    render();
  };
  ask.addEventListener("change", save);
  hide.addEventListener("change", save);
  return [
    head(t("shared.kicker"), x.alias, pill),
    box(t("instrument.idn"),
      el("div", { class: "bar-actions" },
        el("button", { type: "button", class: "iconbtn", title: t("action.identify"), "aria-label": t("action.identify"),
          disabled: down || (a && a.pending), onclick: () => identify(x.alias) }, icon(REFRESH)),
        el("button", { type: "button", class: "iconbtn", title: t("action.open_console"), "aria-label": t("action.open_console"),
          disabled: down, onclick: () => openSharedConsole(x) }, icon(CONSOLE_ICON))),
      // As a local instrument's: the lines on the left, the picture its rules give (pictures.js) on the right, tried on its
      // *IDN? answer (asked by the alias), the alias and the owner's resource name.
      el("div", { class: "ident" },
        el("div", { class: "ident-main" },
          el("dl", { class: "ident-list" },
            identRow(t("field.vendor"), p && p.vendor),
            identRow(t("field.model"), p && p.model),
            identRow(t("field.serial"), p && p.serial),
            identRow(t("field.firmware"), p && p.firmware)),
          a && a.text ? null : el("div", { class: "hint" }, answer),
          why ? el("div", { class: "hint" }, t("shared.probe_said", { error: why })) : null,
          el("div", { class: "ident-opts" },
            el("label", { class: "switch", title: t("shared.idn_hint") }, ask, el("span", {}, t("settings.idn"))),
            el("label", { class: "switch", title: t("shared.hidden_hint") }, hide, el("span", {}, t("settings.hidden"))))),
        pictureArt({ resource: x.alias, members: [{ resource: x.alias }, { resource: x.resource }], parts: bound }))),
    tip(box(t("shared.where"), null,
      facts(
        [t("shared.computer"), x.owner],
        [t("shared.how"), t("shared.kind_relay")],
        [t("shared.resource"), x.resource, true],
        [t("shared.alias"), x.alias, true],
        fresh(x) ? [t("shared.said"), t("shared.said_ago", { n: x.age })] : null)), t("share.others_hint")),
  ];
}
