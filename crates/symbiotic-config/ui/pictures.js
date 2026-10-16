// The user's pictures of instruments: the rules that say which instrument has which, and the picture beside an
// instrument's identification. The editor and the list of them are in picture-manager.js, a window of its own. The
// rules and the files are Rust's (pictures.conf: `pictures`, `picture_data`, `save_picture`, `remove_picture`); the
// regular expressions are this page's.
"use strict";

let pictureConf = { rules: [], conf: null, dir: null };
const pictureImages = new Map(); // file -> a data URL, "loading" or "error"
const patternRegexes = new Map();

const PICTURE_WIDTH = 640; // pixels of a picture, shown at half that

async function loadPictures() {
  try { pictureConf = await invoke("pictures"); } catch (e) { pictureConf = { rules: [], conf: null, dir: null, group: false }; }
}

function escapeRegExp(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// The rule's expression, compiled once; null if it is not one (that rule is left out).
function patternRegex(pattern) {
  if (!patternRegexes.has(pattern)) {
    let re = null;
    try { re = new RegExp(pattern, "i"); } catch (e) { re = null; }
    patternRegexes.set(pattern, re);
  }
  return patternRegexes.get(pattern);
}

// What an instrument is called, for the rules to try: the model it answered *IDN? with (else, for a shared one, the model its
// alias is bound to: `parts`), vendor and model, the name the user gave it, and its resource names.
function pictureTexts(it) {
  const members = it.members ? it.members.map(x => x.resource) : [it.resource];
  const a = idn.get(it.resource);
  const p = a && a.text ? idnParts(a.text) : it.parts || null;
  return [p && p.model, p && [p.vendor, p.model].filter(Boolean).join(" "), settingsOf(it.resource).name, ...members].filter(Boolean);
}

// The first rule, in file order, that matches any of the texts.
function pictureRuleFor(texts) {
  for (const rule of pictureConf.rules) {
    const re = patternRegex(rule.pattern);
    if (re && texts.some(x => re.test(x))) return rule;
  }
  return null;
}

// A rule's picture as a data URL, once it has come from Rust (the page is drawn again when it has).
function pictureSrc(rule) {
  const v = pictureImages.get(rule.file);
  if (v && v.startsWith("data:")) return v;
  if (!v) {
    pictureImages.set(rule.file, "loading");
    invoke("picture_data", { file: rule.file })
      .then(d => { pictureImages.set(rule.file, d); render(); })
      .catch(() => pictureImages.set(rule.file, "error"));
  }
  return null;
}

// The right of the identification: the picture, if a rule gives one. Pictures are managed in their own window (Settings opens it).
function pictureArt(it) {
  const rule = pictureRuleFor(pictureTexts(it));
  const src = rule ? pictureSrc(rule) : null;
  return el("div", { class: "ident-art" }, src ? el("img", { class: rule.dark_keep ? "keep" : null, src, alt: "" }) : null);
}

