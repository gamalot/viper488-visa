//! User-defined aliases for resource names: `DMM` for `GPIB1::6::INSTR` (VPP-4.3 4.3, viOpen, viParseRsrc and
//! viParseRsrcEx).
//!
//! One per line in `aliases.conf` in the configuration directory (`SYMBIOTIC_ALIASES` names another file): `ALIAS =
//! RESOURCE`, `#` comments. An alias has no colon (VPP-4.3's RECOMMENDATION 4.3.6: the colon is the resource names'
//! own) and no white space, and it must not itself be a resource name: a real name always means itself. Aliases compare
//! without regard to case, as resource names do. A line that breaks a rule is ignored; the first of two lines for one
//! alias counts.
//!
//! viFindRsrc lists the names, the aliases or both, as the resource's exposure says (`instruments::Exposure`);
//! viParseRsrcEx gives the alias of one that has one.

use crate::conffile::Field;

use std::path::PathBuf;

pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SYMBIOTIC_ALIASES") {
        return Some(PathBuf::from(p));
    }
    // The configuration directory: shared by all users after an all-users install (paths.rs).
    crate::paths::config_file("aliases.conf")
}

/// Why `name` cannot be an alias, if it cannot.
pub fn check_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("an alias cannot be empty");
    }
    if name.contains(':') {
        return Err("an alias cannot contain a colon (VISA keeps it for resource names)");
    }
    if name.chars().any(|c| c.is_whitespace() || c == '=' || c == '#') {
        return Err("an alias cannot contain white space, '=' or '#'");
    }
    if crate::resource::canonical(name).is_some() {
        return Err("that is a resource name already (COM<n> and ASRL<n> are, for one)");
    }
    Ok(())
}

/// The aliases in `text`: (alias, canonical resource name), in file order.
pub fn parse(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let Some((a, r)) = l.split_once('=') else { continue };
        let (a, r) = (a.trim(), r.trim());
        let Some(r) = crate::resource::canonical(r) else { continue };
        if check_name(a).is_err() || out.iter().any(|(o, _)| o.eq_ignore_ascii_case(a)) {
            continue;
        }
        out.push((a.to_string(), r));
    }
    out
}

pub fn load() -> Vec<(String, String)> {
    path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse(&t)).unwrap_or_default()
}

/// `name` as an alias: the alias as defined, and the resource it stands for.
pub fn resolve(name: &str) -> Option<(String, String)> {
    resolve_in(&load(), name)
}

pub fn resolve_in(list: &[(String, String)], name: &str) -> Option<(String, String)> {
    list.iter().find(|(a, _)| a.eq_ignore_ascii_case(name.trim())).cloned()
}

/// An alias for the resource `name` (any spelling of it), if it has one: the first defined.
pub fn alias_of(name: &str) -> Option<String> {
    alias_of_in(&load(), name)
}

pub fn alias_of_in(list: &[(String, String)], name: &str) -> Option<String> {
    let c = crate::resource::canonical(name)?;
    list.iter().find(|(_, r)| r.eq_ignore_ascii_case(&c)).map(|(a, _)| a.clone())
}

/// Define `alias` as `resource` (replacing it if it exists), or with `None` take it out. Comments
/// and other lines are kept. Pointing an alias that is bound to a unit (identity.rs) at another
/// resource, or taking it out, ends the binding: the alias is then a different instrument's.
pub fn set(alias: &str, resource: Option<&str>) -> std::io::Result<()> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, m.to_string());
    check_name(alias).map_err(bad)?;
    let canonical = match resource {
        Some(r) => Some(crate::resource::canonical(r).ok_or_else(|| bad(&format!("{r:?} is not a resource name")))?),
        None => None,
    };
    let was = resolve(alias).map(|(_, r)| r);
    write_line(alias, canonical.as_deref().map(|r| Field::new(r, &['#'], "a resource name")).transpose()?.as_ref())?;
    if canonical.is_none() || was.as_deref() != canonical.as_deref() {
        crate::identity::set(alias, None)?;
    }
    Ok(())
}

/// Move `alias` to `resource` because the unit it is bound to is there now: the binding stays.
pub fn follow(alias: &str, resource: &str) -> std::io::Result<()> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, m.to_string());
    check_name(alias).map_err(bad)?;
    let r = crate::resource::canonical(resource).ok_or_else(|| bad(&format!("{resource:?} is not a resource name")))?;
    write_line(alias, Some(&Field::new(&r, &['#'], "a resource name")?))
}

/// `ALIAS = RESOURCE` for `alias` (`check_name` has been asked), or no line for it.
fn write_line(alias: &str, resource: Option<&Field>) -> std::io::Result<()> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, m.to_string());
    let alias = Field::new(alias, &['=', '#'], "an alias")?;
    let p = path().ok_or_else(|| bad("no place for aliases.conf (LOCALAPPDATA or HOME is not set)"))?;
    crate::conffile::update(&p, |text| {
        let text = if text.is_empty() { "# Aliases for resource names (symbiotic-ctl alias add/remove): ALIAS = RESOURCE\n" } else { text };
        let mut lines: Vec<String> = text
            .lines()
            .filter(|l| {
                let t = l.trim();
                t.starts_with('#') || t.split_once('=').is_none_or(|(a, _)| !a.trim().eq_ignore_ascii_case(alias.as_str()))
            })
            .map(str::to_string)
            .collect();
        if let Some(r) = resource {
            lines.push(format!("{alias} = {r}"));
        }
        Some(lines.join("\n") + "\n")
    })
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing that would add a line, or split one, gets into the file: refused before it is touched.
    #[test]
    fn no_line_is_added_through_an_alias() {
        let dir = std::env::temp_dir().join(format!("symbiotic-aliases-{}", std::process::id()));
        let file = dir.join("aliases.conf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "DMM = GPIB0::6::INSTR\n").unwrap();
        std::env::set_var("SYMBIOTIC_ALIASES", &file);
        for bad in ["A\u{1b}B", "A\u{85}B", "A\0B"] {
            assert!(set(bad, Some("GPIB0::7::INSTR")).is_err(), "{bad:?}");
            assert!(follow(bad, "GPIB0::7::INSTR").is_err(), "{bad:?}");
        }
        let text = std::fs::read_to_string(&file).unwrap();
        std::env::remove_var("SYMBIOTIC_ALIASES");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(text, "DMM = GPIB0::6::INSTR\n");
    }

    #[test]
    fn a_file_of_aliases() {
        let t = "# comment\n\
                 DMM = gpib1::6::instr\n\
                 scope=USB0::0x0957::0x1792::MY20000003::INSTR\n\
                 dmm = GPIB0::1::INSTR\n\
                 bad:name = GPIB0::2::INSTR\n\
                 COM3 = GPIB0::3::INSTR\n\
                 GPIB0::4::INSTR = GPIB0::5::INSTR\n\
                 nothing = not a resource\n\
                 two words = GPIB0::6::INSTR\n\
                 meter = TCPIP0::198.18.68.63::inst0::INSTR\n";
        let l = parse(t);
        assert_eq!(
            l,
            vec![
                ("DMM".to_string(), "GPIB1::6::INSTR".to_string()),
                ("scope".to_string(), "USB0::0x0957::0x1792::MY20000003::INSTR".to_string()),
                ("meter".to_string(), "TCPIP0::198.18.68.63::inst0::INSTR".to_string()),
            ],
            "the first of two for one alias, and nothing that breaks a rule"
        );
        assert_eq!(resolve_in(&l, "dmm"), Some(("DMM".into(), "GPIB1::6::INSTR".into())), "case does not matter");
        assert_eq!(resolve_in(&l, "GPIB1::6::INSTR"), None, "a resource name is not an alias");
        assert_eq!(alias_of_in(&l, "GPIB1::6::INSTR"), Some("DMM".into()));
        assert_eq!(alias_of_in(&l, "gpib1::6"), Some("DMM".into()), "any spelling of the resource");
        assert_eq!(alias_of_in(&l, "GPIB1::7::INSTR"), None);
    }

    #[test]
    fn names_that_cannot_be_aliases() {
        assert!(check_name("DMM").is_ok());
        assert!(check_name("bench_psu-2").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("a:b").is_err());
        assert!(check_name("my scope").is_err());
        assert!(check_name("COM3").is_err());
        assert!(check_name("ASRL3").is_err());
        assert!(check_name("GPIB0::5::INSTR").is_err());
    }
}
