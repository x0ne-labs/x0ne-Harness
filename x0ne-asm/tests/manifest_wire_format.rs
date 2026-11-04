//! The manifest's bytes across the bincode 2 → postcard 1 codec change.
//!
//! The hex below was produced on 2026-09-23 by `encode_unchecked` built with
//! bincode 2.0.1 `config::standard()`, the codec this crate used before.

use x0ne_asm::ir::{RegexLit, Template, TemplatePart};
use x0ne_asm::manifest::{
    decode, encode_unchecked, Capability, Manifest, ManifestEntry, ManifestError,
};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn entry(pattern: &str, capability: Capability) -> ManifestEntry {
    ManifestEntry {
        pattern: pattern.into(),
        capability,
    }
}

fn small() -> Manifest {
    Manifest {
        version: 1,
        entries: vec![
            entry("/srv/app/**", Capability::ReadWrite),
            entry("/etc/hosts", Capability::Read),
        ],
        regexes: vec![RegexLit {
            pattern: "foo(\\d+)".into(),
            case_insensitive: true,
        }],
        templates: vec![Template {
            parts: vec![TemplatePart::Literal("x-".into()), TemplatePart::Group(1)],
        }],
    }
}

const SMALL_BINCODE2: &str =
    "01020b2f7372762f6170702f2a2a010a2f6574632f686f737473000108666f6f285c642b290101020002782d0101";

/// Every integer and length below 128: bincode 2 and postcard emit the same bytes.
#[test]
fn a_manifest_without_large_integers_keeps_its_exact_bytes() {
    assert_eq!(hex(&encode_unchecked(&small())), SMALL_BINCODE2);
    assert_eq!(decode(&unhex(SMALL_BINCODE2)), Ok(small()));
}

/// A length or group index >= 128 is encoded differently (bincode: 1 byte up
/// to 250, then a 0xfb marker; postcard: LEB128). These two old byte strings
/// are refused as malformed, not read as another manifest.
#[test]
fn old_bytes_with_a_large_integer_are_refused_not_misread() {
    let long = Manifest {
        version: 1,
        entries: vec![entry(
            &format!("/srv/{}", "a".repeat(195)),
            Capability::Read,
        )],
        regexes: vec![],
        templates: vec![],
    };
    let long_bincode2 = format!("0101c82f7372762f{}000000", "61".repeat(195));
    let group = Manifest {
        version: 1,
        entries: vec![entry("/srv/x", Capability::Read)],
        regexes: vec![],
        templates: vec![Template {
            parts: vec![TemplatePart::Group(300)],
        }],
    };
    let group_bincode2 = "0101062f7372762f780000010101fb2c01";

    for (m, old) in [(long, long_bincode2.as_str()), (group, group_bincode2)] {
        let new = encode_unchecked(&m);
        assert_ne!(hex(&new), old, "the wire format differs here");
        assert_eq!(decode(&new), Ok(m));
        assert!(
            matches!(decode(&unhex(old)), Err(ManifestError::Malformed { .. })),
            "bincode 2 bytes must be refused: {:?}",
            decode(&unhex(old))
        );
    }
}
