//! A bound LAN alias followed to its unit's new address by what the configuration window's tree knows (lanunits.rs,
//! `follow_bound`), on real files: a process of its own, so that the configuration directory it points at is no other
//! test's. The case: an instrument bound as AG33210A at 198.18.0.210, given 198.18.0.21.

use symbiotic_core::lanunits::{follow_bound, Place};
use symbiotic_core::{alias, identity};

const ME: &str = "Agilent Technologies,33210A,MY20000002,1.04-1.02-22-2";

fn place(rs: &[&str], online: bool, idn: Option<&str>) -> Place {
    Place { resources: rs.iter().map(|r| r.to_string()).collect(), online, idn: idn.map(str::to_string) }
}

#[test]
fn a_bound_alias_follows_with_its_binding_and_an_unbound_or_wrongly_answered_one_does_not() {
    let dir = std::env::temp_dir().join(format!("sym-follow-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("SYMBIOTIC_CONFIG_DIR", &dir);
    std::fs::write(dir.join("aliases.conf"), "AG33210A = TCPIP0::198.18.0.210::inst0::INSTR\nFREE = TCPIP0::198.18.0.211::inst0::INSTR\n").unwrap();
    std::fs::write(dir.join("identities.conf"), "AG33210A = Agilent Technologies | 33210A | MY20000002\n").unwrap();
    // What the old address keeps, and a setting the new one has of its own.
    std::fs::write(
        dir.join("instruments.conf"),
        "expose-model=2\nTCPIP0::198.18.0.210::5025::SOCKET expose=none\nTCPIP0::198.18.0.210::inst0::INSTR name=bench gen\nTCPIP0::198.18.0.21::inst0::INSTR idn=off\nGPIB0::10::INSTR hidden\n",
    )
    .unwrap();
    std::fs::write(dir.join("lan-units.conf"), "198.18.0.210#0 = Agilent Technologies | 33210A | MY20000002\n").unwrap();

    // An answer that is another unit's moves nothing.
    let wrong = [place(&["TCPIP0::198.18.0.21::inst0::INSTR"], true, Some("Agilent Technologies,33210A,MY00000001,1"))];
    assert!(follow_bound(&wrong).is_empty());
    assert_eq!(alias::resolve("AG33210A").unwrap().1, "TCPIP0::198.18.0.210::inst0::INSTR");

    // Its unit at .21, the old address not in the tree: moved, still bound; the unbound alias stays where it was.
    let tree = [place(&["TCPIP0::198.18.0.21::inst0::INSTR", "TCPIP0::198.18.0.21::5025::SOCKET"], true, Some(ME)), place(&["GPIB0::10::INSTR"], true, Some(ME))];
    let r = follow_bound(&tree);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].moved && r[0].alias == "AG33210A" && r[0].to == ["TCPIP0::198.18.0.21::inst0::INSTR"], "{r:?}");
    assert_eq!(alias::resolve("AG33210A").unwrap().1, "TCPIP0::198.18.0.21::inst0::INSTR");
    assert!(identity::get("AG33210A").is_some(), "the binding stays");
    assert_eq!(alias::resolve("FREE").unwrap().1, "TCPIP0::198.18.0.211::inst0::INSTR");
    // The settings went with it, replacing what .21's inst0 had; .210 keeps nothing, GPIB0::10 is untouched; .21 is recorded, .210's record stays.
    let conf = std::fs::read_to_string(dir.join("instruments.conf")).unwrap();
    assert!(!conf.contains("198.18.0.210"), "{conf}");
    assert!(conf.contains("TCPIP0::198.18.0.21::5025::SOCKET expose=none\n") && conf.contains("TCPIP0::198.18.0.21::inst0::INSTR name=bench gen\n"), "{conf}");
    assert!(!conf.contains("idn=off") && conf.contains("GPIB0::10::INSTR hidden"), "{conf}");
    let mut taken = r[0].settings.clone();
    taken.sort();
    assert_eq!(taken, ["TCPIP0::198.18.0.21::5025::SOCKET", "TCPIP0::198.18.0.21::inst0::INSTR"]);
    assert_eq!(r[0].replaced, ["TCPIP0::198.18.0.21::inst0::INSTR"]);
    // Written under its old name, taken over as units.conf at the first write.
    assert!(!dir.join("lan-units.conf").exists());
    let units = std::fs::read_to_string(dir.join("units.conf")).unwrap();
    assert!(units.contains("198.18.0.210#0 = Agilent Technologies | 33210A | MY20000002") && units.contains("198.18.0.21#0 = Agilent Technologies | 33210A | MY20000002"), "{units}");

    // Where it is now: nothing more to do.
    assert!(follow_bound(&tree).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
