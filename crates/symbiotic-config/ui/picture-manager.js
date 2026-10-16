// The pictures window (picture-manager.js, pictures.html): the list of the user's pictures and the editor that makes a picture of a
// file. The main window opens it (`open_pictures` in src/main.rs) from Settings, and is told when a picture changed.
"use strict";

// ---- drawing a picture: the editor's, and a picture pack's ----------------------------------------------

// An image as a canvas, at most 4000 pixels on a side (a photograph from a camera need not be kept whole; the original is).
function imageCanvas(img) {
  const k = Math.min(1, 4000 / Math.max(img.naturalWidth, img.naturalHeight));
  const c = document.createElement("canvas");
  c.width = Math.max(1, Math.round(img.naturalWidth * k));
  c.height = Math.max(1, Math.round(img.naturalHeight * k));
  c.getContext("2d").drawImage(img, 0, 0, c.width, c.height);
  return c;
}
function loadImageCanvas(url) {
  return new Promise((done, fail) => {
    const img = new Image();
    img.onload = () => done(imageCanvas(img));
    img.onerror = () => fail(new Error(t("picture.load_error")));
    img.src = url;
  });
}

// An image as ink, what a picture is made of before it is scaled: with line art, the darker a pixel the more opaque, and black;
// without, the image itself.
function inkOf(src, lineArt, threshold) {
  if (!lineArt) return src;
  const w = src.width, h = src.height;
  const data = src.getContext("2d").getImageData(0, 0, w, h);
  const px = data.data, hi = threshold, lo = hi - 132;
  for (let i = 0; i < px.length; i += 4) {
    const L = 0.299 * px[i] + 0.587 * px[i + 1] + 0.114 * px[i + 2];
    let a = (hi - L) / (hi - lo);
    a = a < 0 ? 0 : a > 1 ? 1 : a;
    px[i] = px[i + 1] = px[i + 2] = 0;
    px[i + 3] = Math.round(px[i + 3] * a);
  }
  const c = document.createElement("canvas");
  c.width = w;
  c.height = h;
  c.getContext("2d").putImageData(data, 0, 0);
  return c;
}

// Ink at the scale it is drawn at, halved while it is more than twice too big (a straight shrink of a large image drops lines).
function scaledDown(base, scale) {
  const tw = Math.max(1, Math.round(base.width * scale)), th = Math.max(1, Math.round(base.height * scale));
  let c = base;
  while (c.width / 2 >= tw && c.height / 2 >= th) {
    const n = document.createElement("canvas");
    n.width = Math.floor(c.width / 2);
    n.height = Math.floor(c.height / 2);
    const x = n.getContext("2d");
    x.imageSmoothingQuality = "high";
    x.drawImage(c, 0, 0, n.width, n.height);
    c = n;
  }
  const f = document.createElement("canvas");
  f.width = tw;
  f.height = th;
  const x = f.getContext("2d");
  x.imageSmoothingQuality = "high";
  x.drawImage(c, 0, 0, tw, th);
  return f;
}

// What was done to an image (the parameters kept with a picture), with what is missing or out of range made good: an image
// with none is all of it across the picture's width, as line art.
function pictureParams(p, src, darkKeep) {
  const num = (v, d) => (typeof v === "number" && isFinite(v) ? v : d);
  p = p || {};
  const lineArt = p.lineArt !== false;
  return {
    zoom: Math.max(10, Math.min(400, num(p.zoom, 100))), panX: num(p.panX, 0), panY: num(p.panY, 0),
    height: Math.max(40, Math.min(1200, Math.round(num(p.height, src.height * PICTURE_WIDTH / src.width)))),
    lineArt, threshold: Math.max(80, Math.min(250, num(p.threshold, 192))), keep: typeof p.keep === "boolean" ? p.keep : !!darkKeep,
  };
}

// The picture `p` makes of `src`, as the editor shows it (the dark theme's inversion is the page's, not the picture's).
function renderPicture(src, p) {
  const c = document.createElement("canvas");
  c.width = PICTURE_WIDTH;
  c.height = p.height;
  const scale = (PICTURE_WIDTH / src.width) * p.zoom / 100;
  c.getContext("2d").drawImage(scaledDown(inkOf(src, p.lineArt, p.threshold), scale), Math.round(p.panX), Math.round(p.panY));
  return c;
}

// A line drawing at its full size as one grey level a pixel, 0 where the ink is at least half opaque, 255 elsewhere: what a
// picture pack keeps of it (a one-bit PNG, picpack.rs).
function lineArtGray(src, threshold) {
  const ink = inkOf(src, true, threshold);
  const px = ink.getContext("2d").getImageData(0, 0, ink.width, ink.height).data;
  const g = new Uint8Array(ink.width * ink.height);
  for (let i = 0; i < g.length; i++) g[i] = px[i * 4 + 3] >= 128 ? 0 : 255;
  return { width: ink.width, height: ink.height, base64: bytesBase64(g) };
}
function bytesBase64(b) {
  let s = "";
  for (let i = 0; i < b.length; i += 0x8000) s += String.fromCharCode.apply(null, b.subarray(i, i + 0x8000));
  return btoa(s);
}

// ---- the editor ------------------------------------------------------------------------------------

// A dialog that makes a picture of an image file: the picture is `PICTURE_WIDTH` wide and as high as the viewport is, the image
// is moved (drag) and scaled (wheel, slider) in it, and what is outside it is cropped. With "line art" on, the darker a pixel
// the more opaque, and the lines all black: the ground goes clear, whether it was white, a checkerboard drawn into a JPEG or
// clear already. `rule` is a rule being changed (none: a new one).
function pictureEditor({ rule = null } = {}) {
  // `original`: the image as it came ({url, ext}), kept; `pending`: what was done to it before, to be put back when it is loaded.
  const st = { src: null, ink: null, inkKey: "", scaled: null, zoom: 100, panX: 0, panY: 0, height: 280, lineArt: true, threshold: 192, keep: rule ? rule.dark_keep : false, original: null, pending: null };

  const file = el("input", { type: "file", accept: "image/*", "aria-label": t("picture.file") });
  const out = el("canvas", { class: "picture-view", width: PICTURE_WIDTH, height: st.height });
  const zoom = el("input", { type: "range", min: "10", max: "400", value: "100", "aria-label": t("picture.zoom") });
  const height = el("input", { type: "number", min: "40", max: "1200", step: "1", class: "narrow", "aria-label": t("picture.height") });
  const fit = el("button", { type: "button", onclick: () => { if (st.src) fitImage(); } }, t("picture.fit"));
  const lineArt = el("input", { type: "checkbox", checked: true });
  const threshold = el("input", { type: "range", min: "80", max: "250", value: String(st.threshold), "aria-label": t("picture.threshold") });
  const keep = el("input", { type: "checkbox", checked: st.keep });
  const pattern = el("input", { type: "text", class: "mono", spellcheck: "false", autocomplete: "off", "aria-label": t("picture.pattern") });
  pattern.value = rule ? rule.pattern : "";
  const note = el("div", { class: "hint", role: "status" });
  const message = el("div", { class: "error", role: "alert" });
  const save = el("button", { type: "button", class: "primary", disabled: true }, t("action.save"));
  const cancel = el("button", { type: "button", onclick: () => d.close() }, t("action.cancel"));
  const remove = rule ? el("button", { type: "button", class: "danger-button" }, t("action.remove")) : null;

  const body = el("div", { class: "picture-editor" },
    el("div", { class: "hint" }, t("picture.hint")),
    el("div", { class: "row" }, file),
    el("div", { class: "picture-frame" }, out),
    el("div", { class: "row" },
      el("label", { class: "inline" }, el("span", { class: "k" }, t("picture.zoom")), zoom),
      el("label", { class: "inline" }, el("span", { class: "k" }, t("picture.height")), height),
      fit),
    el("label", { class: "switch" }, lineArt, el("span", {}, el("span", {}, t("picture.lineart")), el("br"), el("span", { class: "hint" }, t("picture.lineart_hint")))),
    el("div", { class: "row" }, el("label", { class: "inline" }, el("span", { class: "k" }, t("picture.threshold")), threshold)),
    el("label", { class: "switch" }, keep, el("span", {}, el("span", {}, t("picture.keep")), el("br"), el("span", { class: "hint" }, t("picture.keep_hint")))),
    el("div", { class: "k" }, t("picture.pattern")),
    el("div", { class: "row" }, pattern),
    note,
    message);
  const d = el("dialog", { class: "dialog wide" },
    el("h2", {}, t("picture.title")),
    body,
    el("div", { class: "row end" }, remove, el("span", { class: "spacer" }), cancel, save));
  d.addEventListener("close", () => d.remove());
  document.body.append(d);
  d.showModal();

  // The kind of file a data URL is, as the name the original is kept under; null for one that is not kept as it came.
  function extOf(url) {
    const m = /^data:image\/(png|jpeg|gif|webp|bmp)[;,]/.exec(url);
    return m ? (m[1] === "jpeg" ? "jpg" : m[1]) : null;
  }

  function setSource(img) {
    st.src = imageCanvas(img);
    st.inkKey = "";
    st.scaled = null;
    const before = st.pending;
    st.pending = null;
    if (before) applyParams(before); else fitImage();
  }
  function loadUrl(url) {
    const img = new Image();
    img.onload = () => setSource(img);
    img.onerror = () => { message.textContent = t("picture.load_error"); };
    img.src = url;
  }
  function loadFile(f) {
    if (!f) return;
    message.textContent = "";
    const r = new FileReader();
    r.onload = () => {
      // A new image: kept as it came, and seen whole.
      st.original = { url: String(r.result), ext: extOf(String(r.result)) };
      st.pending = null;
      loadUrl(String(r.result));
    };
    r.onerror = () => { message.textContent = t("picture.load_error"); };
    r.readAsDataURL(f);
  }

  // The image as ink, and that at the scale it is drawn at, each kept until what it depends on changes.
  function ink() {
    const key = st.lineArt ? "L" + st.threshold : "raw";
    if (st.inkKey === key) return st.ink;
    st.ink = inkOf(st.src, st.lineArt, st.threshold);
    st.inkKey = key;
    st.scaled = null;
    return st.ink;
  }
  function scaledInk(scale) {
    const base = ink();
    const key = st.inkKey + "@" + scale.toFixed(4);
    if (st.scaled && st.scaled.key === key) return st.scaled.canvas;
    st.scaled = { key, canvas: scaledDown(base, scale) };
    return st.scaled.canvas;
  }

  function draw() {
    out.width = PICTURE_WIDTH;
    out.height = st.height;
    out.classList.toggle("invert", st.lineArt && !st.keep);
    if (st.src) {
      try {
        const scale = (PICTURE_WIDTH / st.src.width) * st.zoom / 100;
        out.getContext("2d").drawImage(scaledInk(scale), Math.round(st.panX), Math.round(st.panY));
      } catch (e) { message.textContent = String(e); }
    }
    update();
  }

  // The image across the picture's width, as high as that makes it (the picture shows all of it at 100 % and its own height).
  function fitHeight() {
    return st.src.height * PICTURE_WIDTH / st.src.width;
  }

  // What is shown is a part of the original, or all of it (zoomed out, the whole image with a clear margin round it). Where the image is
  // smaller than the view it is in the middle; where it is larger, it cannot be moved off the view's edges, so no gap opens between
  // its edge and the view's. Every change of zoom, height or position goes through here; both are idempotent, so zooming in and out
  // again comes back to the same picture.
  function constrain() {
    if (!st.src) return;
    st.height = Math.max(40, Math.min(1200, Math.round(st.height)));
    st.zoom = Math.max(10, Math.min(400, st.zoom));
    const w = PICTURE_WIDTH * st.zoom / 100, h = fitHeight() * st.zoom / 100;
    st.panX = w >= PICTURE_WIDTH ? Math.min(0, Math.max(PICTURE_WIDTH - w, st.panX)) : (PICTURE_WIDTH - w) / 2;
    st.panY = h >= st.height ? Math.min(0, Math.max(st.height - h, st.panY)) : (st.height - h) / 2;
  }

  function syncControls() {
    zoom.value = String(Math.round(st.zoom));
    height.value = String(st.height);
  }

  // The whole image across the picture's width, as high as it comes out.
  function fitImage() {
    st.zoom = 100;
    st.panX = 0;
    st.panY = 0;
    st.height = Math.floor(fitHeight());
    constrain();
    syncControls();
    draw();
  }

  // What was done to an image before (the parameters kept with the picture), done again.
  function applyParams(p) {
    const num = (v, d) => (typeof v === "number" && isFinite(v) ? v : d);
    st.lineArt = p.lineArt !== false;
    st.threshold = Math.max(80, Math.min(250, num(p.threshold, 192)));
    st.keep = !!p.keep;
    st.zoom = num(p.zoom, 100);
    st.panX = num(p.panX, 0);
    st.panY = num(p.panY, 0);
    st.height = num(p.height, Math.floor(fitHeight()));
    st.inkKey = "";
    lineArt.checked = st.lineArt;
    threshold.value = String(st.threshold);
    threshold.disabled = !st.lineArt;
    keep.checked = st.keep;
    constrain();
    syncControls();
    draw();
  }

  // Zoom about a point of the picture (the picture's own pixels): that point stays where it is, as far as the image's edges allow.
  function zoomAbout(next, cx, cy) {
    const was = st.zoom;
    st.zoom = Math.max(10, Math.min(400, next));
    const k = st.zoom / was;
    st.panX = cx - (cx - st.panX) * k;
    st.panY = cy - (cy - st.panY) * k;
    constrain();
    syncControls();
    draw();
  }

  // Does the pattern make a rule, and which instruments here does it reach?
  function update() {
    const text = pattern.value.trim();
    let problem = null, re = null;
    if (!text) problem = t("picture.pattern_empty");
    else { try { re = new RegExp(text, "i"); } catch (e) { problem = t("picture.pattern_invalid", { error: e.message }); } }
    if (problem) {
      note.textContent = problem;
      note.classList.add("error");
    } else {
      note.classList.remove("error");
      const names = managerInstruments.filter(it => it.texts.some(x => re.test(x))).map(it => it.name);
      note.textContent = names.length ? t("picture.matches", { list: names.join(", ") }) : t("picture.matches_none");
    }
    save.disabled = !!problem || !st.src;
  }

  file.addEventListener("change", () => loadFile(file.files[0]));
  d.addEventListener("dragover", e => e.preventDefault());
  d.addEventListener("drop", e => { e.preventDefault(); loadFile(e.dataTransfer && e.dataTransfer.files[0]); });
  zoom.addEventListener("input", () => { if (st.src) zoomAbout(Number(zoom.value), PICTURE_WIDTH / 2, st.height / 2); });
  height.addEventListener("input", () => {
    const v = Math.round(Number(height.value));
    if (v >= 40 && v <= 1200) { st.height = v; constrain(); syncControls(); draw(); }
  });
  lineArt.addEventListener("change", () => {
    st.lineArt = lineArt.checked;
    // A picture that is not line art keeps its colours on the dark theme.
    st.keep = !st.lineArt;
    keep.checked = st.keep;
    threshold.disabled = !st.lineArt;
    if (st.src) draw();
  });
  threshold.addEventListener("input", () => { st.threshold = Number(threshold.value); if (st.src) draw(); });
  keep.addEventListener("change", () => { st.keep = keep.checked; draw(); });
  pattern.addEventListener("input", update);
  out.addEventListener("wheel", e => {
    if (!st.src) return;
    e.preventDefault();
    const k = PICTURE_WIDTH / out.clientWidth;
    zoomAbout(st.zoom * (e.deltaY < 0 ? 1.1 : 1 / 1.1), e.offsetX * k, e.offsetY * k);
  }, { passive: false });
  let drag = null;
  out.addEventListener("pointerdown", e => { if (st.src) { drag = { x: e.clientX, y: e.clientY }; out.setPointerCapture(e.pointerId); } });
  out.addEventListener("pointermove", e => {
    if (!drag) return;
    const k = PICTURE_WIDTH / out.clientWidth;
    st.panX += (e.clientX - drag.x) * k;
    st.panY += (e.clientY - drag.y) * k;
    drag = { x: e.clientX, y: e.clientY };
    constrain();
    draw();
  });
  out.addEventListener("pointerup", () => { drag = null; });
  out.addEventListener("pointercancel", () => { drag = null; });

  save.addEventListener("click", async () => {
    message.textContent = "";
    save.disabled = true;
    try {
      // The original goes with the picture, as it came (a kind of file that is not kept as it is, as the PNG the page made of it), and
      // what was done to it: the picture can be made again from all of the image. A rule being changed is replaced by this one (in
      // its place), not left beside it.
      const original = st.original && st.original.ext ? st.original : { url: st.src.toDataURL("image/png"), ext: "png" };
      const edit = JSON.stringify({ zoom: st.zoom, panX: st.panX, panY: st.panY, height: st.height, lineArt: st.lineArt, threshold: st.threshold, keep: st.keep });
      const saved = await invoke("save_picture", {
        pattern: pattern.value.trim(), darkKeep: st.keep, pngBase64: out.toDataURL("image/png"), replaces: rule ? rule.pattern : null,
        originalBase64: original.url, originalExt: original.ext, edit,
      });
      pictureImages.delete(saved);
      d.close();
      await changed();
      toast(t("picture.saved"));
    } catch (e) {
      message.textContent = String(e);
      save.disabled = false;
    }
  });
  if (remove) {
    remove.addEventListener("click", async () => {
      if (!await confirmDialog(t("picture.remove_title"), t("picture.remove_text", { pattern: rule.pattern }), t("action.remove"), true)) return;
      try {
        await invoke("remove_picture", { pattern: rule.pattern });
        pictureImages.delete(rule.file);
        d.close();
        await changed();
        toast(t("picture.removed"));
      } catch (e) { message.textContent = String(e); }
    });
  }

  // A picture that keeps its colours is not turned into line art when it is changed.
  if (rule && rule.dark_keep) {
    st.lineArt = false;
    lineArt.checked = false;
    threshold.disabled = true;
  }
  syncControls();
  draw();
  // Changing a picture starts from the original it was made of, with what was done to it: all of the image is there to
  // be moved, scaled and cropped again. A picture without one starts from itself.
  if (rule) {
    invoke("picture_original", { file: rule.file }).then(async o => {
      if (o) {
        st.original = { url: o.data_url, ext: extOf(o.data_url) };
        try { st.pending = o.edit ? JSON.parse(o.edit) : null; } catch (e) { st.pending = null; }
        loadUrl(o.data_url);
      } else {
        const url = await invoke("picture_data", { file: rule.file });
        st.original = { url, ext: "png" };
        loadUrl(url);
      }
    }).catch(e => { message.textContent = String(e); });
  }
}

// ---- the window ----------------------------------------------------------------------------------------

// The pictures as a grid of cards, in the rules' order (the first that matches is an instrument's): the picture, its pattern and its
// file; a card opens the editor.
function managerList() {
  const cards = pictureConf.rules.map(rule => {
    const src = pictureSrc(rule);
    return el("button", { type: "button", class: "picture-card", title: t("picture.edit"), onclick: () => pictureEditor({ rule }) },
      el("span", { class: "thumb" }, src ? el("img", { class: rule.dark_keep ? "keep" : null, src, alt: "" }) : null),
      el("span", { class: "mono pattern" }, rule.pattern),
      el("span", { class: "hint" }, rule.file + (rule.dark_keep ? " \u00b7 " + t("picture.keep_short") : "") + (patternRegex(rule.pattern) ? "" : " \u00b7 " + t("picture.rule_invalid"))));
  });
  return [cards.length ? el("div", { class: "picture-grid" }, cards) : el("div", { class: "hint" }, t("settings.pictures_none"))];
}

let managerInstruments = []; // [{name, texts}]: what the editor tries a pattern on, given by the main window

// Once joined to a group the pictures are the group's: the registry keeps them, and every computer's copy follows. This computer's own
// (from before it joined) are moved there by hand; a pattern the group has a picture for already is skipped, and stays here.
function groupBar() {
  const move = el("button", { type: "button", title: t("picture.group_move_hint"), onclick: async () => {
    move.disabled = true;
    try {
      const done = await invoke("pictures_adopt");
      const skipped = done.filter(x => x.problem);
      toast(done.length ? t("picture.group_moved", { n: done.length - skipped.length }) + (skipped.length ? " " + t("picture.group_skipped", { list: skipped.map(x => x.pattern + " (" + x.problem + ")").join("; ") }) : "") : t("picture.group_nothing"), skipped.length > 0);
      await changed();
    } catch (e) { toast(String(e), true); }
    move.disabled = false;
  } }, t("picture.group_move"));
  return el("div", { class: "row" }, el("span", { class: "hint" }, t("picture.group_hint")), move);
}

// ---- picture packs ----------------------------------------------------------------------------------------

let packBusy = false;
// Every picture here, in a pack in the Downloads folder: the image each was made of (a line drawing as one bit a pixel)
// and what was done to it. One with no image of its own goes as the picture itself, line art as it is.
async function exportPack() {
  packBusy = true;
  render();
  try {
    const items = [];
    for (const r of pictureConf.rules) {
      let o = await invoke("picture_original", { file: r.file }).catch(() => null);
      let edit = null;
      try { edit = o && o.edit ? JSON.parse(o.edit) : null; } catch (e) { edit = null; }
      if (!o) o = { data_url: await invoke("picture_data", { file: r.file }) };
      const src = await loadImageCanvas(o.data_url);
      const p = pictureParams(edit || (o.edit === undefined ? { zoom: 100, panX: 0, panY: 0, height: src.height, lineArt: true } : null), src, r.dark_keep);
      const item = { pattern: r.pattern, dark_keep: r.dark_keep, file: r.file, edit: JSON.stringify(p) };
      if (p.lineArt) item.gray = lineArtGray(src, p.threshold);
      else { item.original = o.data_url; item.ext = (/^data:image\/(\w+)/.exec(o.data_url) || [, "png"])[1].replace("jpeg", "jpg"); }
      items.push(item);
    }
    const path = await invoke("export_picture_pack", { items });
    toast(t("picture.pack_exported", { n: items.length, path }));
  } catch (e) { toast(String(e), true); }
  packBusy = false;
  render();
}

// A pack's pictures made here, each as the editor would of its image (picture_pack read in Rust): a rule here already, by the
// same pattern, is left as it is.
async function importPack(file) {
  if (!file) return;
  packBusy = true;
  render();
  try {
    const data = await new Promise((done, fail) => {
      const r = new FileReader();
      r.onload = () => done(String(r.result));
      r.onerror = () => fail(new Error(t("picture.load_error")));
      r.readAsDataURL(file);
    });
    const entries = await invoke("read_picture_pack", { base64: data });
    const have = new Set(pictureConf.rules.map(r => r.pattern));
    const skipped = [];
    let added = 0;
    for (const e of entries) {
      if (have.has(e.pattern)) { skipped.push(e.pattern); continue; }
      let edit = null;
      try { edit = e.edit ? JSON.parse(e.edit) : null; } catch (x) { edit = null; }
      const src = await loadImageCanvas(e.original);
      const p = pictureParams(edit, src, e.dark_keep);
      await invoke("save_picture", {
        pattern: e.pattern, darkKeep: p.keep, pngBase64: renderPicture(src, p).toDataURL("image/png"), replaces: null,
        originalBase64: e.original, originalExt: e.ext, edit: JSON.stringify(p),
      });
      added++;
    }
    await changed();
    toast(!entries.length ? t("picture.pack_empty") : t("picture.pack_imported", { n: added }) + (skipped.length ? " " + t("picture.pack_skipped", { list: skipped.join(", ") }) : ""));
  } catch (e) { toast(String(e), true); }
  packBusy = false;
  render();
}

// The list is drawn again after a picture is saved or removed, and tells the main window, whose identifications show them.
function render() {
  const h1 = el("h1", { title: t("settings.pictures_hint") }, t("settings.pictures"));
  document.title = t("settings.pictures");
  statusLeft(t("count.pictures", { n: pictureConf.rules.length }));
  // replaceChildren writes a null as the text "null" (el() leaves it out; this does not): not in a group, there is no bar.
  document.getElementById("pictures").replaceChildren(...[
    el("div", { class: "console-head" }, h1, el("span", { class: "spacer" }),
      el("button", { type: "button", title: t("picture.pack_import_hint"), disabled: packBusy, onclick: () => packFile.click() }, t("picture.pack_import")),
      el("button", { type: "button", title: t("picture.pack_export_hint"), disabled: packBusy || !pictureConf.rules.length, onclick: exportPack }, t("picture.pack_export")),
      el("button", { type: "button", disabled: packBusy, onclick: () => pictureEditor() }, t("picture.add"))),
    pictureConf.group ? groupBar() : null,
    el("div", { class: "picture-list" }, ...managerList()),
    // At the window's foot, under a line, as Settings has the configuration folder at its box's.
    ...(pictureConf.dir ? [el("div", { class: "divider" }), pathLine(t("settings.pictures_folder"), pictureConf.dir)] : [])].filter(Boolean));
}
// The file chooser for a pack: one, kept out of the page's drawing (render would make a new one each time).
const packFile = el("input", { type: "file", accept: ".zip,application/zip", class: "offscreen", "aria-hidden": "true",
  onchange: () => { const f = packFile.files[0]; packFile.value = ""; importPack(f); } });
document.body.append(packFile);

async function changed() {
  await loadPictures();
  render();
  await window.__TAURI__.event.emit("pictures-changed", {});
}

async function main() {
  await startWindow();
  statusBar();
  managerInstruments = await invoke("pictures_init");
  await loadPictures();
  render();
  setWindowTitle(t("settings.pictures"));

  followChanges(render);
  // The main window opened this one again: the instruments may have changed.
  window.__TAURI__.event.listen("pictures-instruments", e => { managerInstruments = e.payload || []; });
}
main().catch(e => {
  document.getElementById("pictures").replaceChildren(el("div", { class: "empty error" }, "The window could not start: " + String(e)));
});
