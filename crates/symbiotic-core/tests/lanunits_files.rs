//! A LAN address that has changed owner, settled on real files (lanunits.rs): a process of its own, so that the
//! configuration directory it points at is no other test's. The case: one instrument had 198.18.68.57 and left its
//! settings there; another got the address.

use std::path::Path;
use symbiotic_core::{alias, identity, instruments, lanunits};

const KS: &str = "Keysight Technologies,34465A,MY20000001,A.03.03";
const SDG: &str = "Siglent Technologies,SDG1062X Plus,SDG1EXAMPLE001,V1P.1.1.1.43R8";

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

fn start(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    write(dir, "instruments.conf", "expose-model=2\nTCPIP0::198.18.68.57::inst0::INSTR expose=none\nTCPIP0::198.18.68.57::5025::SOCKET expose=none name=bench\nGPIB0::6::INSTR idn=off\n");
    write(dir, "aliases.conf", "OLD = TCPIP0::198.18.68.57::hislip0::INSTR\nSDG1062X+ = TCPIP0::198.18.68.57::inst0::INSTR\n");
    write(dir, "identities.conf", "SDG1062X+ = Siglent Technologies | SDG1062X Plus | SDG1EXAMPLE001\n");
    write(dir, "lan-units.conf", "198.18.68.57#0 = Keysight Technologies | 34465A | MY20000001\n");
}

fn line(r: &str) -> Option<String> {
    let text = std::fs::read_to_string(instruments::path().unwrap()).unwrap();
    text.lines().find(|l| l.starts_with(r)).map(str::to_string)
}

#[test]
fn an_address_that_changed_owner_is_said_then_kept_cleared_or_moved() {
    let dir = std::env::temp_dir().join(format!("sym-lanunits-files-{}", std::process::id()));
    std::env::set_var("SYMBIOTIC_CONFIG_DIR", &dir);
    let sdg = "TCPIP0::198.18.68.57::inst0::INSTR";

    // Said, and nothing written.
    start(&dir);
    let before = std::fs::read_to_string(dir.join("instruments.conf")).unwrap();
    match lanunits::seen(sdg, SDG).unwrap() {
        lanunits::Seen::Changed { was } => assert_eq!((was.model.as_str(), was.serial.as_str()), ("34465A", "MY20000001")),
        s => panic!("{s:?}"),
    }
    assert_eq!(std::fs::read_to_string(dir.join("instruments.conf")).unwrap(), before);
    let k = lanunits::kept_now("198.18.68.57#0");
    assert_eq!(k.settings.len(), 2);
    assert_eq!(k.aliases.iter().map(|a| (a.alias.as_str(), a.bound)).collect::<Vec<_>>(), [("OLD", false), ("SDG1062X+", true)]);

    // Keep: only the record changes.
    lanunits::keep(sdg, SDG).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("instruments.conf")).unwrap(), before);
    assert_eq!(lanunits::seen(sdg, SDG).unwrap(), lanunits::Seen::Fine);

    // Clear: the settings go, the aliases and the other instruments' lines stay.
    start(&dir);
    lanunits::clear(sdg, SDG).unwrap();
    assert_eq!(line("TCPIP0::198.18.68.57"), None);
    assert_eq!(line("GPIB0::6::INSTR").as_deref(), Some("GPIB0::6::INSTR idn=off"));
    assert_eq!(alias::resolve("OLD").unwrap().1, "TCPIP0::198.18.68.57::hislip0::INSTR");
    assert_eq!(lanunits::seen(sdg, SDG).unwrap(), lanunits::Seen::Fine);

    // Move to where the 34465A is now: its settings and its unbound alias go along; the bound one stays and keeps its unit.
    start(&dir);
    write(&dir, "instruments.conf", &(std::fs::read_to_string(dir.join("instruments.conf")).unwrap() + "TCPIP0::198.18.68.66::inst0::INSTR idn=off\n"));
    lanunits::move_to(sdg, SDG, "198.18.68.66").unwrap();
    assert_eq!(line("TCPIP0::198.18.68.57"), None);
    assert_eq!(line("TCPIP0::198.18.68.66::inst0::INSTR").as_deref(), Some("TCPIP0::198.18.68.66::inst0::INSTR expose=none"), "the moved one replaces what was there");
    assert_eq!(line("TCPIP0::198.18.68.66::5025::SOCKET").as_deref(), Some("TCPIP0::198.18.68.66::5025::SOCKET expose=none name=bench"));
    assert_eq!(alias::resolve("OLD").unwrap().1, "TCPIP0::198.18.68.66::hislip0::INSTR");
    assert_eq!(alias::resolve("SDG1062X+").unwrap().1, sdg);
    assert!(identity::get("SDG1062X+").is_some(), "the bound alias keeps its binding");
    assert_eq!(lanunits::seen(sdg, SDG).unwrap(), lanunits::Seen::Fine);
    assert_eq!(lanunits::seen("TCPIP0::198.18.68.66::hislip0::INSTR", KS).unwrap(), lanunits::Seen::Fine, "the 34465A is recorded where it went");
    assert!(lanunits::move_to(sdg, SDG, "198.18.68.57").is_err(), "not to its own address");

    // Removing an instrument takes its settings and its records; another host's stay, and a host with an alias left keeps its MAC.
    start(&dir);
    write(&dir, "units.conf", "mac 198.18.68.57 = 02:00:00:00:00:62\nmac 198.18.68.66 = 02:00:00:00:00:66\n198.18.68.66#0 = Keysight Technologies | 34465A | MY20000001\n");
    write(&dir, "instruments.conf", &(std::fs::read_to_string(dir.join("instruments.conf")).unwrap() + "TCPIP0::198.18.68.66::inst0::INSTR expose=none\n"));
    lanunits::forget(&["TCPIP0::198.18.68.66::inst0::INSTR".to_string(), "TCPIP0::198.18.68.66::hislip0::INSTR".to_string()]).unwrap();
    assert_eq!(line("TCPIP0::198.18.68.66"), None);
    assert!(line("TCPIP0::198.18.68.57::inst0::INSTR").is_some());
    let units = std::fs::read_to_string(dir.join("units.conf")).unwrap();
    assert!(!units.contains("198.18.68.66"), "{units}");
    assert!(units.contains("mac 198.18.68.57"), "{units}");

    let _ = std::fs::remove_dir_all(&dir);
}
