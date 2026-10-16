//! Per-resource settings for the configuration window and for what `viFindRsrc` lists: whether it may be sent `*IDN?`
//! to learn what it is, a display name of the user's own, whether the window hides it, and how it is exposed to VISA
//! users: by its name, by its alias, by both, or not at all.
//!
//! One line per resource in `instruments.conf` in the configuration directory (`SYMBIOTIC_INSTRUMENTS` names another
//! file): `RESOURCE [idn=off] [hidden] [expose=name|alias|both|none] [name=TEXT]`, `#` comments; `name=` takes the rest
//! of the line. A resource with no line is sent `*IDN?`, has no display name and the default exposure: the file holds
//! only what differs. A line whose resource is not a resource name is ignored; the first of two lines for one resource
//! counts.
//!
//! Two lines are not a resource's: `list=all` makes `viFindRsrc` list the instruments that are not there (a gateway's
//! GPIB address with no listener) as well, and `expose-model=2` says the window has turned older `hidden` lines, which
//! also kept an instrument out of `viFindRsrc`, into `expose=none` lines.
//!
//! The instruments other computers of a sharing group share have lines of their own, by alias: `shared:ALIAS [idn=on]
//! [hidden]`. One is not sent `*IDN?` unless it says so: it is another computer's instrument, reached through its
//! owner, who may be using it.
//!
//! Some instruments predate IEEE 488.2 and answer `*IDN?` with an error, a beep or nothing; hence the per-instrument
//! switch. Nothing here sends anything.

use std::path::PathBuf;

/// How a resource is exposed to VISA users: what `viFindRsrc` lists for it (`viOpen` takes the name and the
/// alias whatever this says).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expose {
    Name,
    Alias,
    Both,
    None,
}

impl Expose {
    pub fn parse(s: &str) -> Option<Expose> {
        match s.trim().to_ascii_lowercase().as_str() {
            "name" => Some(Expose::Name),
            "alias" => Some(Expose::Alias),
            "both" => Some(Expose::Both),
            "none" => Some(Expose::None),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Expose::Name => "name",
            Expose::Alias => "alias",
            Expose::Both => "both",
            Expose::None => "none",
        }
    }
}

/// What differs from the default for one resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// May it be sent `*IDN?` (default: yes).
    pub idn: bool,
    /// The user's name for it, shown instead of its model.
    pub name: Option<String>,
    /// Left out of the window's tree (and not sent `*IDN?`) until shown again.
    pub hidden: bool,
    /// What `viFindRsrc` lists for it; `None`: the default (its alias if it has one, else its name).
    pub expose: Option<Expose>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { idn: true, name: None, hidden: false, expose: None }
    }
}

pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SYMBIOTIC_INSTRUMENTS") {
        return Some(PathBuf::from(p));
    }
    crate::paths::config_file("instruments.conf")
}

/// The settings in `text`: (canonical resource name, settings), in file order.
pub fn parse(text: &str) -> Vec<(String, Settings)> {
    let mut out: Vec<(String, Settings)> = Vec::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (first, mut rest) = l.split_once(char::is_whitespace).unwrap_or((l, ""));
        let Some(resource) = crate::resource::canonical(first) else { continue };
        if out.iter().any(|(r, _)| *r == resource) {
            continue;
        }
        let mut s = Settings::default();
        loop {
            rest = rest.trim_start();
            if let Some(n) = rest.strip_prefix("name=") {
                let n = n.trim();
                s.name = (!n.is_empty()).then(|| n.to_string());
                break;
            }
            let Some(word) = rest.split_whitespace().next() else { break };
            match word {
                "idn=off" => s.idn = false,
                "idn=on" => s.idn = true,
                "hidden" => s.hidden = true,
                w if w.starts_with("expose=") => s.expose = Expose::parse(&w["expose=".len()..]),
                _ => {} // a word this version does not know: kept by set(), ignored here
            }
            rest = &rest[word.len()..];
        }
        out.push((resource, s));
    }
    out
}

pub fn load() -> Vec<(String, Settings)> {
    path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse(&t)).unwrap_or_default()
}

/// One instrument's settings (the defaults if it has no line).
pub fn get(resource: &str) -> Settings {
    let Some(r) = crate::resource::canonical(resource) else { return Settings::default() };
    load().into_iter().find(|(x, _)| *x == r).map(|(_, s)| s).unwrap_or_default()
}

const HEADER: &str = "# Per-resource settings (the configuration window): RESOURCE [idn=off] [hidden] [expose=name|alias|both|none] [name=TEXT]; list=all: viFindRsrc lists absent instruments too\n";

/// The line that makes `viFindRsrc` list the instruments that are not there.
const LIST_ALL: &str = "list=all";

/// The line that says `hidden` has been turned into `expose=none` lines (see `migrate_hidden`).
const MODEL_2: &str = "expose-model=2";

/// Does `text` ask `viFindRsrc` to list the absent instruments too?
pub fn list_all_in(text: &str) -> bool {
    has_line(text, LIST_ALL)
}

fn has_line(text: &str, word: &str) -> bool {
    text.lines().any(|l| l.trim().eq_ignore_ascii_case(word))
}

/// The setting: is `list=all` in the file?
pub fn list_all() -> bool {
    path().and_then(|p| std::fs::read_to_string(p).ok()).is_some_and(|t| list_all_in(&t))
}

static SEES_ALL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// This process wants every instrument from `viFindRsrc`, by name, whatever the settings say: the
/// configuration window's tree is made of what the search finds, and it has to show the ones that are not
/// exposed (to expose them again) and the absent ones (as idle), and has no use for aliases as items.
pub fn see_everything() {
    SEES_ALL.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Did this process ask for `see_everything`: the configuration window, whose tree has no use for the aliases
/// `viFindRsrc` adds for other programs either.
pub fn sees_everything() -> bool {
    SEES_ALL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Does `viFindRsrc` in this process list the absent instruments: the setting, or `see_everything`.
pub fn find_lists_all() -> bool {
    SEES_ALL.load(std::sync::atomic::Ordering::Relaxed) || list_all()
}

/// Turn `viFindRsrc`'s leaving out of absent instruments off (`on`) or back on.
pub fn set_list_all(on: bool) -> std::io::Result<()> {
    set_line(LIST_ALL, on)
}

/// Put the one-word line `word` in the file, or take it out; the rest of the file stays as it was.
fn set_line(word: &str, on: bool) -> std::io::Result<()> {
    let p = path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no place for instruments.conf (LOCALAPPDATA or HOME is not set)"))?;
    crate::conffile::update(&p, |text| {
        let text = if text.is_empty() { HEADER } else { text };
        let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().eq_ignore_ascii_case(word)).collect();
        if on {
            lines.push(word);
        }
        Some(lines.join("\n") + "\n")
    })
    .map(|_| ())
}

/// What the window does with an instrument another computer shares (see the top).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharedView {
    /// Asked `*IDN?` when the window opens or refreshes (default: no).
    pub idn: bool,
    /// Left out of the window's tree until shown again.
    pub hidden: bool,
}

const SHARED: &str = "shared:";

/// The shared instruments' lines in `text`: (alias, settings), in file order; the first of two for one alias counts.
pub fn parse_shared(text: &str) -> Vec<(String, SharedView)> {
    let mut out: Vec<(String, SharedView)> = Vec::new();
    for l in text.lines().map(str::trim) {
        let mut w = l.split_whitespace();
        let Some(alias) = w.next().and_then(|f| f.strip_prefix(SHARED)).filter(|a| !a.is_empty()) else { continue };
        if out.iter().any(|(a, _)| a.eq_ignore_ascii_case(alias)) {
            continue;
        }
        let mut v = SharedView::default();
        for word in w {
            match word {
                "idn=on" => v.idn = true,
                "idn=off" => v.idn = false,
                "hidden" => v.hidden = true,
                _ => {}
            }
        }
        out.push((alias.to_string(), v));
    }
    out
}

pub fn load_shared() -> Vec<(String, SharedView)> {
    path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse_shared(&t)).unwrap_or_default()
}

/// Set a shared instrument's settings: its line is replaced (or removed, when they are the defaults); the rest is kept.
pub fn set_shared(alias: &str, v: SharedView) -> std::io::Result<()> {
    let bad = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
    if alias.is_empty() || alias.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad(format!("{alias:?} is not an alias")));
    }
    let p = path().ok_or_else(|| bad("no place for instruments.conf (LOCALAPPDATA or HOME is not set)".into()))?;
    crate::conffile::update(&p, |text| {
        let text = if text.is_empty() { HEADER } else { text };
        let mut lines: Vec<String> = text
            .lines()
            .filter(|l| !l.split_whitespace().next().and_then(|f| f.strip_prefix(SHARED)).is_some_and(|a| a.eq_ignore_ascii_case(alias)))
            .map(str::to_string)
            .collect();
        if v != SharedView::default() {
            lines.push(format!("{SHARED}{alias}{}{}", if v.idn { " idn=on" } else { "" }, if v.hidden { " hidden" } else { "" }));
        }
        Some(lines.join("\n") + "\n")
    })
    .map(|_| ())
}

/// What the window calls one instrument, for the resources that are one: a host's `inst<n>`,
/// `hislip<n>` and a socket (the window shows them as one, and a `hidden` on any of them hides it); a
/// gateway's `gpib0,N` and everything else is its own.
fn group_key(canonical: &str) -> String {
    use crate::lan::name;
    let Some(t) = name::parse(canonical) else { return canonical.to_string() };
    if t.gpib().is_some() {
        return canonical.to_string();
    }
    let dev = t.device.to_ascii_lowercase();
    let index = if t.class() == "SOCKET" {
        Some(0)
    } else {
        dev.strip_prefix("inst").or_else(|| dev.strip_prefix("hislip")).and_then(|n| n.parse::<u32>().ok())
    };
    match index {
        Some(i) => format!("TCPIP#{}#{i}", t.host.to_ascii_lowercase()),
        None => canonical.to_string(),
    }
}

/// The instruments marked `hidden` in `text`, to ask of any resource name.
pub struct Hidden(Vec<String>);

impl Hidden {
    pub fn in_text(text: &str) -> Hidden {
        Hidden(parse(text).into_iter().filter(|(_, s)| s.hidden).map(|(r, _)| group_key(&r)).collect())
    }

    pub fn load() -> Hidden {
        Hidden::in_text(&path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default())
    }

    /// Is `resource` (any spelling of a resource name) one of the hidden instruments?
    pub fn hides(&self, resource: &str) -> bool {
        !self.0.is_empty() && crate::resource::canonical(resource).is_some_and(|r| self.0.contains(&group_key(&r)))
    }
}

/// Which resource names each is exposed by, for `viFindRsrc`: the settings of every resource and the aliases, read
/// once.
pub struct Exposure {
    settings: Vec<(String, Settings)>,
    aliases: Vec<(String, String)>,
    /// Before the window has turned `hidden` into `expose=none` (no `expose-model=2` line): the hidden instruments,
    /// which are then left out of the search.
    legacy: Option<Hidden>,
}

impl Exposure {
    pub fn in_text(text: &str, aliases: Vec<(String, String)>) -> Exposure {
        Exposure { settings: parse(text), aliases, legacy: (!has_line(text, MODEL_2)).then(|| Hidden::in_text(text)) }
    }

    pub fn load() -> Exposure {
        let text = path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        // A network instrument's alias is the group's on a computer in a group (share::names).
        Exposure::in_text(&text, [crate::alias::load(), crate::share::lookup::lan_aliases()].concat())
    }

    /// `load`, for the resources a search `found`: a LAN host's settings count only while it is the instrument they are
    /// for, by its MAC, and go with that MAC where it went (`lanunits::reconcile`). A host that cannot be told is
    /// listed as if it had none.
    pub fn load_for(found: &[String]) -> Exposure {
        let r = crate::lanunits::reconcile(found);
        let mut e = Exposure::load();
        if !r.unconfirmed.is_empty() {
            e.settings.retain(|(res, _)| !crate::lanunits::host_of(res).is_some_and(|h| r.unconfirmed.contains(&h)));
            if let Some(h) = &mut e.legacy {
                h.0.retain(|k| !r.unconfirmed.iter().any(|u| k.starts_with(&format!("TCPIP#{u}#")) || crate::lanunits::host_of(k).as_deref() == Some(u.as_str())));
            }
        }
        e
    }

    fn aliases_of(&self, canonical: &str) -> Vec<String> {
        self.aliases.iter().filter(|(_, r)| r.eq_ignore_ascii_case(canonical)).map(|(a, _)| a.clone()).collect()
    }

    /// How the resource `canonical` is exposed: its own setting, else (before the model-2 turn) not at all
    /// if it was hidden, else by its alias if it has one and by its name if not.
    pub fn mode(&self, canonical: &str) -> Expose {
        if let Some(e) = self.settings.iter().find(|(r, _)| r == canonical).and_then(|(_, s)| s.expose) {
            return e;
        }
        if self.legacy.as_ref().is_some_and(|h| h.hides(canonical)) {
            return Expose::None;
        }
        if self.aliases_of(canonical).is_empty() { Expose::Name } else { Expose::Alias }
    }

    /// What `viFindRsrc` lists for `found` (resource names it found): for each, in its order, its name, its
    /// aliases, both (the name first) or nothing. A resource that is to be listed by its alias and has none is
    /// listed by its name: it would be gone otherwise.
    pub fn listed(&self, found: Vec<String>) -> Vec<String> {
        let mut out = Vec::new();
        for n in found {
            let c = crate::resource::canonical(&n).unwrap_or_else(|| n.clone());
            let aliases = self.aliases_of(&c);
            match self.mode(&c) {
                Expose::None => {}
                Expose::Name => out.push(n),
                Expose::Alias if aliases.is_empty() => out.push(n),
                Expose::Alias => out.extend(aliases),
                Expose::Both => {
                    out.push(n);
                    out.extend(aliases);
                }
            }
        }
        out
    }
}

/// The first start of the window with the expose model: every resource of a hidden instrument (`resources`: the ones
/// the window knows, and the hidden lines' own) that has no `expose=` of its own gets `expose=none`, so nothing a
/// program saw before it sees now; then `expose-model=2` says it is done. Does nothing once that line is there.
pub fn migrate_hidden(resources: &[String]) -> std::io::Result<()> {
    let p = path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no place for instruments.conf (LOCALAPPDATA or HOME is not set)"))?;
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    if has_line(&text, MODEL_2) {
        return Ok(());
    }
    let hidden = Hidden::in_text(&text);
    let have = parse(&text);
    let mut all: Vec<String> = resources.iter().filter_map(|r| crate::resource::canonical(r)).collect();
    all.extend(have.iter().filter(|(_, s)| s.hidden).map(|(r, _)| r.clone()));
    all.sort();
    all.dedup();
    for r in all {
        if !hidden.hides(&r) {
            continue;
        }
        let mut s = have.iter().find(|(x, _)| *x == r).map(|(_, s)| s.clone()).unwrap_or_default();
        if s.expose.is_none() {
            s.expose = Some(Expose::None);
            set(&r, &s)?;
        }
    }
    set_line(MODEL_2, true)
}

fn line(resource: &str, s: &Settings) -> Option<String> {
    let mut l = resource.to_string();
    if !s.idn {
        l.push_str(" idn=off");
    }
    if s.hidden {
        l.push_str(" hidden");
    }
    if let Some(e) = s.expose {
        l.push_str(" expose=");
        l.push_str(e.word());
    }
    if let Some(n) = &s.name {
        l.push_str(" name=");
        l.push_str(n);
    }
    (l.len() > resource.len()).then_some(l)
}

/// Set one instrument's settings: its line is replaced (or removed, when they are the defaults);
/// comments and other lines are kept.
pub fn set(resource: &str, s: &Settings) -> std::io::Result<()> {
    let bad = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
    let r = crate::resource::canonical(resource).ok_or_else(|| bad(format!("{resource:?} is not a resource name")))?;
    // `name=` is the line's last field and takes spaces; it may not take a new line.
    if let Some(n) = &s.name {
        crate::conffile::Field::new(n, &[], "a display name")?;
    }
    let p = path().ok_or_else(|| bad("no place for instruments.conf (LOCALAPPDATA or HOME is not set)".into()))?;
    crate::conffile::update(&p, |text| {
        let text = if text.is_empty() { HEADER } else { text };
        let mut lines: Vec<String> = text
            .lines()
            .filter(|l| {
                let t = l.trim();
                t.starts_with('#') || t.split_whitespace().next().and_then(crate::resource::canonical).is_none_or(|x| x != r)
            })
            .map(str::to_string)
            .collect();
        if let Some(l) = line(&r, s) {
            lines.push(l);
        }
        Some(lines.join("\n") + "\n")
    })
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests that point SYMBIOTIC_INSTRUMENTS at a file of their own take turns.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn lines_give_settings_and_the_rest_is_ignored() {
        let t = "# c\n\
                 gpib0::20::instr idn=off name=Left bench generator\n\
                 GPIB0::6::INSTR name=The DMM  \n\
                 TCPIP0::198.18.68.65::inst0::INSTR idn=off hidden\n\
                 not-a-resource idn=off\n\
                 GPIB0::20::INSTR name=second line for it\n\
                 USB0::0x0957::0x1792::MY20000003::INSTR future=thing idn=off\n";
        let p = parse(t);
        assert_eq!(p.len(), 4, "{p:?}");
        assert_eq!(p[0], ("GPIB0::20::INSTR".to_string(), Settings { idn: false, name: Some("Left bench generator".into()), hidden: false, expose: None }));
        assert_eq!(p[1].1, Settings { idn: true, name: Some("The DMM".into()), hidden: false, expose: None });
        assert_eq!(p[2], ("TCPIP0::198.18.68.65::inst0::INSTR".to_string(), Settings { idn: false, name: None, hidden: true, expose: None }));
        assert_eq!(p[3].1, Settings { idn: false, name: None, hidden: false, expose: None }, "an unknown word is skipped");
    }

    #[test]
    fn defaults_make_no_line() {
        assert_eq!(line("GPIB0::20::INSTR", &Settings::default()), None);
        assert_eq!(
            line("GPIB0::20::INSTR", &Settings { idn: false, name: Some("x y".into()), hidden: true, expose: None }).as_deref(),
            Some("GPIB0::20::INSTR idn=off hidden name=x y")
        );
    }

    #[test]
    fn a_shared_instrument_has_a_line_by_its_alias_that_no_resource_reads() {
        let _turn = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("symbiotic-instruments-shared-{}", std::process::id()));
        let file = dir.join("instruments.conf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "GPIB0::6::INSTR hidden\nshared:SDS824 hidden\n").unwrap();
        std::env::set_var("SYMBIOTIC_INSTRUMENTS", &file);
        set_shared("sds824", SharedView { idn: true, hidden: false }).unwrap();
        set_shared("KS2002A", SharedView { idn: false, hidden: true }).unwrap();
        let shared = load_shared();
        let resources = load();
        set_shared("KS2002A", SharedView::default()).unwrap();
        let after = load_shared();
        assert!(set_shared("two words", SharedView::default()).is_err());
        std::env::remove_var("SYMBIOTIC_INSTRUMENTS");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(shared, vec![("sds824".to_string(), SharedView { idn: true, hidden: false }), ("KS2002A".to_string(), SharedView { idn: false, hidden: true })]);
        assert_eq!(resources.len(), 1, "the resources' settings do not see the shared lines: {resources:?}");
        assert_eq!(after.len(), 1, "back to the defaults, the line goes");
        assert!(SharedView::default() == SharedView { idn: false, hidden: false }, "not asked unless the user says so");
    }

    #[test]
    fn a_display_name_is_one_line() {
        let _turn = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("symbiotic-instruments-name-{}", std::process::id()));
        let file = dir.join("instruments.conf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "GPIB0::6::INSTR name=The DMM\n").unwrap();
        std::env::set_var("SYMBIOTIC_INSTRUMENTS", &file);
        for bad in ["x\nGPIB0::7::INSTR hidden", "x\ry", "x\u{1b}[31m"] {
            let s = Settings { name: Some(bad.into()), ..Settings::default() };
            assert!(set("GPIB0::20::INSTR", &s).is_err(), "{bad:?}");
        }
        let text = std::fs::read_to_string(&file).unwrap();
        std::env::remove_var("SYMBIOTIC_INSTRUMENTS");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(text, "GPIB0::6::INSTR name=The DMM\n");
    }

    #[test]
    fn set_replaces_one_line_and_keeps_the_rest() {
        let _turn = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("symbiotic-instruments-{}", std::process::id()));
        let file = dir.join("instruments.conf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "# mine\nGPIB0::6::INSTR name=The DMM\ngpib0::20::instr idn=off\n").unwrap();
        std::env::set_var("SYMBIOTIC_INSTRUMENTS", &file);
        set("GPIB0::20::INSTR", &Settings { idn: true, name: Some("Generator".into()), hidden: false, expose: None }).unwrap();
        set("GPIB0::6::INSTR", &Settings::default()).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        std::env::remove_var("SYMBIOTIC_INSTRUMENTS");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(text, "# mine\nGPIB0::20::INSTR name=Generator\n");
    }

    #[test]
    fn hidden_goes_by_the_instrument_the_window_shows() {
        let h = Hidden::in_text(
            "ASRL14::INSTR hidden\nTCPIP0::10.0.0.7::hislip0::INSTR hidden\nTCPIP0::gw::gpib0,29::INSTR hidden\nGPIB0::6::INSTR name=x\n",
        );
        assert!(h.hides("asrl14::instr") && h.hides("ASRL14"));
        assert!(!h.hides("ASRL1::INSTR"));
        // One instrument on three protocols is hidden whole; a second one on the host is not.
        assert!(h.hides("TCPIP0::10.0.0.7::inst0::INSTR") && h.hides("TCPIP::10.0.0.7") && h.hides("TCPIP0::10.0.0.7::5025::SOCKET"));
        assert!(!h.hides("TCPIP0::10.0.0.7::inst1::INSTR") && !h.hides("TCPIP0::10.0.0.8::inst0::INSTR"));
        // A gateway's addresses are each their own.
        assert!(h.hides("TCPIP0::gw::gpib0,29::INSTR") && !h.hides("TCPIP0::gw::gpib0,6::INSTR"));
        assert!(!h.hides("GPIB0::6::INSTR"));
        assert!(!Hidden::in_text("").hides("ASRL1::INSTR"));
    }

    #[test]
    fn list_all_is_one_line_the_rest_keeps() {
        assert!(!list_all_in("# c\nASRL1::INSTR hidden\n") && list_all_in("# c\n  LIST=ALL \n"));
        let _turn = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("symbiotic-listall-{}", std::process::id()));
        let file = dir.join("instruments.conf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "# mine\nASRL1::INSTR hidden\n").unwrap();
        std::env::set_var("SYMBIOTIC_INSTRUMENTS", &file);
        set_list_all(true).unwrap();
        set("GPIB0::6::INSTR", &Settings { idn: true, name: Some("DMM".into()), hidden: false, expose: None }).unwrap();
        let on = (list_all(), std::fs::read_to_string(&file).unwrap());
        set_list_all(false).unwrap();
        let off = (list_all(), std::fs::read_to_string(&file).unwrap());
        std::env::remove_var("SYMBIOTIC_INSTRUMENTS");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(on.0 && on.1.contains("list=all\n") && on.1.contains("ASRL1::INSTR hidden"), "{on:?}");
        assert_eq!(off, (false, "# mine\nASRL1::INSTR hidden\nGPIB0::6::INSTR name=DMM\n".to_string()));
    }

    #[test]
    fn expose_sits_before_the_name_and_round_trips() {
        let t = "ASRL1::INSTR expose=none\nTCPIP0::10.0.0.7::hislip0::INSTR hidden expose=Both name=The 34465A\nGPIB0::6::INSTR expose=bogus\n";
        let p = parse(t);
        assert_eq!(p[0].1.expose, Some(Expose::None));
        assert_eq!((p[1].1.hidden, p[1].1.expose, p[1].1.name.as_deref()), (true, Some(Expose::Both), Some("The 34465A")));
        assert_eq!(p[2].1.expose, None, "a word it does not know is no setting");
        assert_eq!(line("ASRL1::INSTR", &p[0].1).as_deref(), Some("ASRL1::INSTR expose=none"));
        assert_eq!(line("TCPIP0::10.0.0.7::hislip0::INSTR", &p[1].1).as_deref(), Some("TCPIP0::10.0.0.7::hislip0::INSTR hidden expose=both name=The 34465A"));
    }

    fn exposure(text: &str, aliases: &str) -> Exposure {
        Exposure::in_text(text, crate::alias::parse(aliases))
    }

    #[test]
    fn what_the_search_lists_for_each_exposure() {
        let found = || ["GPIB0::6::INSTR", "TCPIP0::10.0.0.7::inst0::INSTR", "TCPIP0::10.0.0.7::hislip0::INSTR", "ASRL3::INSTR", "ASRL4::INSTR"].map(String::from).to_vec();
        let ex = exposure(
            "expose-model=2\n\
             TCPIP0::10.0.0.7::hislip0::INSTR expose=both\n\
             TCPIP0::10.0.0.7::inst0::INSTR expose=name\n\
             ASRL3::INSTR expose=none\n\
             ASRL4::INSTR expose=alias\n",
            "DMM = GPIB0::6::INSTR\nHi = TCPIP0::10.0.0.7::hislip0::INSTR\nLow = TCPIP0::10.0.0.7::inst0::INSTR\n",
        );
        assert_eq!(
            ex.listed(found()),
            // GPIB0::6: no setting, has an alias: the alias alone. inst0: its name though it has an alias. hislip0: both.
            // ASRL3: not at all. ASRL4: wants its alias and has none: its name.
            ["DMM", "TCPIP0::10.0.0.7::inst0::INSTR", "TCPIP0::10.0.0.7::hislip0::INSTR", "Hi", "ASRL4::INSTR"]
        );
        // With no settings at all: the name, and the alias in its place where there is one.
        assert_eq!(exposure("expose-model=2\n", "").listed(found()), found());
    }

    #[test]
    fn before_the_window_has_migrated_hidden_still_hides() {
        let text = "ASRL1::INSTR hidden\nTCPIP0::10.0.0.7::hislip0::INSTR hidden\nASRL2::INSTR hidden expose=name\n";
        let ex = exposure(text, "");
        let found = ["ASRL1::INSTR", "ASRL2::INSTR", "ASRL3::INSTR", "TCPIP0::10.0.0.7::inst0::INSTR", "TCPIP0::10.0.0.7::5025::SOCKET", "TCPIP0::10.0.0.8::inst0::INSTR"].map(String::from).to_vec();
        // The window's own word for ASRL2 wins; the hidden instrument's other protocols go with it.
        assert_eq!(ex.listed(found.clone()), ["ASRL2::INSTR", "ASRL3::INSTR", "TCPIP0::10.0.0.8::inst0::INSTR"]);
        // After the migration the line says it is done, and `hidden` is the window's alone.
        let done = exposure(&format!("{text}expose-model=2\n"), "");
        assert_eq!(done.listed(found.clone()), found);
    }

    #[test]
    fn migrating_hidden_writes_none_for_each_resource_of_a_hidden_instrument_once() {
        let _turn = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("symbiotic-migrate-{}", std::process::id()));
        let file = dir.join("instruments.conf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "# mine\nASRL1::INSTR hidden\nTCPIP0::10.0.0.7::hislip0::INSTR hidden name=Box\nASRL2::INSTR hidden expose=name\nGPIB0::6::INSTR name=DMM\n").unwrap();
        std::env::set_var("SYMBIOTIC_INSTRUMENTS", &file);
        let known: Vec<String> = ["TCPIP0::10.0.0.7::inst0::INSTR", "TCPIP0::10.0.0.7::hislip0::INSTR", "TCPIP0::10.0.0.7::5025::SOCKET", "TCPIP0::10.0.0.7::inst1::INSTR", "GPIB0::6::INSTR"].map(String::from).to_vec();
        migrate_hidden(&known).unwrap();
        let once = std::fs::read_to_string(&file).unwrap();
        migrate_hidden(&known).unwrap();
        let twice = std::fs::read_to_string(&file).unwrap();
        let after: Vec<(String, Settings)> = load();
        std::env::remove_var("SYMBIOTIC_INSTRUMENTS");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(once, twice, "done once");
        assert!(once.contains("expose-model=2") && once.starts_with("# mine"), "{once}");
        let e = |r: &str| after.iter().find(|(x, _)| x == r).and_then(|(_, s)| s.expose);
        for r in ["ASRL1::INSTR", "TCPIP0::10.0.0.7::inst0::INSTR", "TCPIP0::10.0.0.7::hislip0::INSTR", "TCPIP0::10.0.0.7::5025::SOCKET"] {
            assert_eq!(e(r), Some(Expose::None), "{r}");
        }
        assert_eq!(e("ASRL2::INSTR"), Some(Expose::Name), "its own setting stays");
        assert_eq!(e("TCPIP0::10.0.0.7::inst1::INSTR"), None, "another instrument on the host");
        assert_eq!(e("GPIB0::6::INSTR"), None);
        assert!(after.iter().find(|(x, _)| x == "TCPIP0::10.0.0.7::hislip0::INSTR").is_some_and(|(_, s)| s.hidden && s.name.as_deref() == Some("Box")));
    }
}
